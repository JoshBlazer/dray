//! A spawned task that is aborted when its owner is dropped.

/// A spawned task tied to the lifetime of this handle.
///
/// `tokio::spawn` detaches: dropping a `JoinHandle` leaves the task running.
/// That is not merely a leak here. A heartbeat outliving its attempt goes on
/// renewing a lease for work that has stopped, so the job never expires, is
/// never reaped, and is never retried — it is simply lost. A memory sampler
/// outliving its subprocess polls a `/proc` entry that now belongs to whatever
/// process next takes that pid.
///
/// So nothing in this crate is allowed to detach.
#[derive(Debug)]
pub(crate) struct TaskGuard<T>(tokio::task::JoinHandle<T>);

impl<T: Send + 'static> TaskGuard<T> {
    pub(crate) fn spawn(future: impl std::future::Future<Output = T> + Send + 'static) -> Self {
        Self(tokio::spawn(future))
    }

    /// The underlying handle, for awaiting the task's completion.
    pub(crate) fn handle(&mut self) -> &mut tokio::task::JoinHandle<T> {
        &mut self.0
    }
}

impl<T> Drop for TaskGuard<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    /// The bug the chaos test found: a detached heartbeat outlived the worker
    /// that started it and went on renewing the lease for work that had
    /// stopped, so the job was never reaped and never came back.
    #[tokio::test]
    async fn a_guarded_task_stops_when_its_guard_is_dropped() {
        let ticks = Arc::new(AtomicUsize::new(0));

        let guard = {
            let ticks = Arc::clone(&ticks);
            TaskGuard::spawn(async move {
                loop {
                    ticks.fetch_add(1, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
        };

        tokio::time::sleep(Duration::from_millis(80)).await;
        let while_alive = ticks.load(Ordering::SeqCst);
        assert!(while_alive > 0, "the task should have run at all");

        drop(guard);

        tokio::time::sleep(Duration::from_millis(80)).await;
        let after_drop = ticks.load(Ordering::SeqCst);
        assert!(
            after_drop <= while_alive + 1,
            "the task kept running after its guard was dropped: {while_alive} -> {after_drop}"
        );
    }
}
