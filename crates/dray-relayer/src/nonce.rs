//! Single-writer nonce management for one relayer account.
//!
//! # What goes wrong without it
//!
//! An account's transactions are ordered by nonce, and the chain executes them
//! strictly in sequence. Two consequences follow, and both are worse than they
//! first sound:
//!
//! - **A gap blocks everything behind it.** If nonces 5 and 7 are broadcast but
//!   6 never is, 7 sits in the mempool indefinitely. Not rejected — *waiting*,
//!   for a transaction that is never coming.
//! - **A duplicate replaces.** Two transactions sharing a nonce are not both
//!   mined; the node keeps one. So two concurrent submissions that each read
//!   "the next nonce is 5" produce one settlement, not two, and the relayer
//!   believes it made both.
//!
//! Reading the nonce from the chain per submission does not fix this. A pending
//! transaction is included in `pending` counts only once the node has seen it,
//! so two submissions issued closely enough together read the same value.
//!
//! # The approach
//!
//! One process owns one key, and therefore one nonce sequence — which is why
//! Dray gives each relayer in the permissioned set its own account (ADR-011).
//! Within the process, [`NonceManager`] holds a lock that is taken for the
//! *whole* submission, not just for allocating the number.
//!
//! That is the part worth being deliberate about. Serialising allocation alone
//! would let two submissions reserve 5 and 6 and broadcast out of order; if the
//! one holding 5 then failed permanently, 6 would wait for ever behind a
//! transaction that will never exist. Holding the lock across the broadcast
//! means a nonce is only ever consumed by a transaction that actually reached
//! the node.
//!
//! Throughput is not the thing being optimised here. One account can only have
//! one transaction mined at a time in any case.

use tokio::sync::{Mutex, MutexGuard};

/// The next nonce for one account, and the lock that serialises its use.
#[derive(Debug, Default)]
pub struct NonceManager {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    /// `None` until read from the chain, and set back to `None` whenever the
    /// chain disagrees with us.
    next: Option<u64>,
}

impl NonceManager {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the lock for one submission.
    ///
    /// Held until the guard is dropped, which must be *after* the transaction
    /// has been broadcast. See the module documentation for why allocating
    /// under the lock and broadcasting outside it is not enough.
    pub async fn begin(&self) -> NonceGuard<'_> {
        NonceGuard {
            state: self.state.lock().await,
        }
    }

    /// The next nonce, without taking the lock. For metrics and logging only.
    pub async fn peek(&self) -> Option<u64> {
        self.state.lock().await.next
    }
}

/// Exclusive access to an account's nonce for the duration of one submission.
#[derive(Debug)]
pub struct NonceGuard<'a> {
    state: MutexGuard<'a, State>,
}

impl NonceGuard<'_> {
    /// The nonce to use, if the manager already knows it.
    ///
    /// `None` means the caller must read it from the chain and report it with
    /// [`NonceGuard::synchronise`]. That happens on the first submission after
    /// start-up, and after any failure that suggested the counter had drifted.
    #[must_use]
    pub fn nonce(&self) -> Option<u64> {
        self.state.next
    }

    /// Adopt the value just read from the chain.
    ///
    /// Callers should read the *pending* count, not the latest: a transaction
    /// this relayer broadcast moments ago is pending, not mined, and starting
    /// from the mined count would reuse its nonce and silently replace it.
    pub fn synchronise(&mut self, from_chain: u64) {
        self.state.next = Some(from_chain);
    }

    /// Record that the nonce was consumed by a transaction the node accepted.
    ///
    /// Called only after a successful broadcast. A nonce advanced on an attempt
    /// that never reached the node would leave a permanent gap, and every
    /// later transaction from this account would wait behind it.
    pub fn consumed(&mut self) {
        self.state.next = self.state.next.map(|n| n.saturating_add(1));
    }

    /// Discard the cached value, forcing a re-read from the chain next time.
    ///
    /// For failures that mean the counter has drifted — "nonce too low" says a
    /// transaction landed that this relayer did not know about. Waiting does
    /// not fix that; only asking the chain does.
    pub fn invalidate(&mut self) {
        self.state.next = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::Arc, time::Duration};

    #[tokio::test]
    async fn a_fresh_manager_knows_nothing_until_told() {
        let manager = NonceManager::new();
        assert_eq!(manager.begin().await.nonce(), None);
        assert_eq!(manager.peek().await, None);
    }

    #[tokio::test]
    async fn a_synchronised_nonce_is_used_then_advanced() {
        let manager = NonceManager::new();

        {
            let mut guard = manager.begin().await;
            guard.synchronise(41);
            assert_eq!(guard.nonce(), Some(41));
            guard.consumed();
        }

        assert_eq!(manager.begin().await.nonce(), Some(42));
    }

    /// A nonce advanced for a transaction that never reached the node leaves a
    /// gap, and every later transaction from the account waits behind it for
    /// ever.
    #[tokio::test]
    async fn a_failed_broadcast_does_not_consume_the_nonce() {
        let manager = NonceManager::new();

        {
            let mut guard = manager.begin().await;
            guard.synchronise(7);
            // Broadcast fails; `consumed` is deliberately not called.
        }

        assert_eq!(
            manager.begin().await.nonce(),
            Some(7),
            "the nonce must still be available to the next attempt"
        );
    }

    #[tokio::test]
    async fn invalidating_forces_a_re_read() {
        let manager = NonceManager::new();

        {
            let mut guard = manager.begin().await;
            guard.synchronise(10);
            guard.consumed();
        }

        {
            let mut guard = manager.begin().await;
            assert_eq!(guard.nonce(), Some(11));
            guard.invalidate();
        }

        assert_eq!(
            manager.begin().await.nonce(),
            None,
            "after a nonce disagreement the chain is the only authority"
        );
    }

    /// The property the whole module exists for. Concurrent submissions must
    /// receive distinct, consecutive nonces — a duplicate silently replaces a
    /// transaction, and a gap blocks every later one.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_submissions_get_distinct_consecutive_nonces() {
        let manager = Arc::new(NonceManager::new());
        manager.begin().await.synchronise(100);

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let manager = Arc::clone(&manager);
            tasks.push(tokio::spawn(async move {
                let mut guard = manager.begin().await;
                let nonce = guard.nonce().expect("already synchronised");

                // Stand in for the broadcast: the lock must still be held.
                tokio::time::sleep(Duration::from_millis(1)).await;

                guard.consumed();
                nonce
            }));
        }

        let mut issued = Vec::new();
        for task in tasks {
            issued.push(task.await.expect("task should not panic"));
        }
        issued.sort_unstable();

        assert_eq!(
            issued,
            (100..132).collect::<Vec<_>>(),
            "nonces must be distinct and consecutive, with no gaps and no reuse"
        );
    }

    /// The lock is held across the broadcast, not merely across allocation.
    /// Overlapping submissions would let two transactions be broadcast out of
    /// nonce order.
    #[tokio::test(flavor = "multi_thread")]
    async fn submissions_do_not_overlap() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let manager = Arc::new(NonceManager::new());
        manager.begin().await.synchronise(0);

        let in_flight = Arc::new(AtomicUsize::new(0));
        let overlaps = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let manager = Arc::clone(&manager);
            let in_flight = Arc::clone(&in_flight);
            let overlaps = Arc::clone(&overlaps);

            tasks.push(tokio::spawn(async move {
                let mut guard = manager.begin().await;

                if in_flight.fetch_add(1, Ordering::SeqCst) != 0 {
                    overlaps.fetch_add(1, Ordering::SeqCst);
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
                in_flight.fetch_sub(1, Ordering::SeqCst);

                guard.consumed();
            }));
        }

        for task in tasks {
            task.await.expect("task should not panic");
        }

        assert_eq!(
            overlaps.load(Ordering::SeqCst),
            0,
            "two submissions were in flight at once for one account"
        );
    }

    #[tokio::test]
    async fn an_absurd_nonce_does_not_overflow() {
        let manager = NonceManager::new();
        let mut guard = manager.begin().await;
        guard.synchronise(u64::MAX);
        guard.consumed();
        assert_eq!(guard.nonce(), Some(u64::MAX), "saturates rather than wraps");
    }
}
