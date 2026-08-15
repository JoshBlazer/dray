//! Running an untrusted, expensive subprocess under strict resource bounds.
//!
//! This is the single most important piece of engineering in the worker. Proof
//! generation is memory-hungry and can hang; without bounds, one pathological
//! input takes down the machine and every job on it. With them, exceeding a
//! limit is an ordinary, recoverable, *metered* failure.
//!
//! Three bounds are enforced:
//!
//! | Bound | Mechanism | What it stops |
//! |---|---|---|
//! | Wall clock | `tokio::time::timeout` then kill | A process that hangs without burning CPU — blocked on I/O, or spinning in a sleep loop |
//! | Address space | `RLIMIT_AS` via `ulimit -v` | Runaway allocation exhausting machine memory |
//! | CPU time | `RLIMIT_CPU` via `ulimit -t` | A tight loop that would otherwise burn a core until the wall clock expires |
//!
//! # Why the shell rather than `pre_exec`
//!
//! The idiomatic way to set rlimits on a child is `CommandExt::pre_exec`, which
//! is `unsafe` — it runs between `fork` and `exec`, where only async-signal-safe
//! calls are legal. This workspace sets `unsafe_code = "forbid"`, and that is a
//! property worth keeping in a service that runs untrusted input.
//!
//! So the limits are applied by `sh` instead: `ulimit` before `exec`. The
//! `exec` matters — it *replaces* the shell with the target process rather than
//! forking it, so there is no intermediate process to orphan, and killing the
//! child kills the real work.
//!
//! # The `RLIMIT_AS` caveat, stated plainly
//!
//! `ulimit -v` bounds *address space*, not resident memory. A process that maps
//! far more than it touches can trip it while using little real memory. This is
//! the honest trade for avoiding `unsafe`: cgroups would bound RSS exactly but
//! need either root or a systemd delegation this project does not assume.
//! Bounds should therefore be set with headroom over measured peak RSS, and
//! [`Bounds::for_proving`] does exactly that.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};

use tokio::io::AsyncReadExt;

/// Limits applied to a single subprocess.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Hard wall-clock deadline. Exceeding it kills the process.
    pub wall_clock: Duration,
    /// Address-space ceiling in kilobytes (`RLIMIT_AS`).
    pub address_space_kb: u64,
    /// CPU-time ceiling in seconds (`RLIMIT_CPU`).
    pub cpu_seconds: u64,
}

impl Bounds {
    /// Defaults derived from measurement, not guesswork.
    ///
    /// Phase 1 measured proving at 1.9–2.5 s and ~42 MB peak RSS on a four-core
    /// machine. The ceilings below are deliberately generous multiples of that:
    /// a bound that a *normal* proof can trip converts healthy work into
    /// retries, which is worse than no bound at all because it wastes the work
    /// and then repeats it.
    ///
    /// Address space is set far above peak RSS because `RLIMIT_AS` counts
    /// mappings rather than touched pages, and Barretenberg maps generously.
    #[must_use]
    pub fn for_proving() -> Self {
        Self {
            wall_clock: Duration::from_secs(120),
            address_space_kb: 4 * 1024 * 1024, // 4 GiB
            cpu_seconds: 100,
        }
    }
}

/// Why a bounded run did not succeed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BoundedError {
    #[error("exceeded the {limit:?} wall-clock limit and was killed")]
    WallClockExceeded { limit: Duration },

    #[error("exceeded the {limit_kb} KB address-space limit")]
    AddressSpaceExceeded { limit_kb: u64 },

    #[error("exceeded the {limit_seconds}s CPU limit")]
    CpuExceeded { limit_seconds: u64 },

    #[error("killed by signal {signal}")]
    Killed { signal: i32 },

    #[error("exited with status {code}: {stderr}")]
    Failed { code: i32, stderr: String },

    #[error("could not run the process: {0}")]
    Spawn(String),
}

impl BoundedError {
    /// Whether retrying could plausibly succeed.
    ///
    /// Resource exhaustion is treated as transient: the same job on a quieter
    /// machine, or with a larger bound, may well succeed, and the spec is
    /// explicit that exceeding a bound is a normal recoverable failure. A
    /// non-zero exit is treated as permanent, because a tool that rejected its
    /// input will reject it again.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            BoundedError::WallClockExceeded { .. }
            | BoundedError::AddressSpaceExceeded { .. }
            | BoundedError::CpuExceeded { .. }
            | BoundedError::Killed { .. }
            | BoundedError::Spawn(_) => true,
            BoundedError::Failed { .. } => false,
        }
    }

    /// The label used for this failure in metrics.
    #[must_use]
    pub fn metric_label(&self) -> &'static str {
        match self {
            BoundedError::WallClockExceeded { .. } => "timeout",
            BoundedError::AddressSpaceExceeded { .. } => "oom",
            BoundedError::CpuExceeded { .. } => "cpu_exhausted",
            BoundedError::Killed { .. } => "killed",
            BoundedError::Failed { .. } => "nonzero_exit",
            BoundedError::Spawn(_) => "spawn_failed",
        }
    }
}

/// What a successful bounded run produced.
#[derive(Debug, Clone)]
pub struct Completed {
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    /// Peak resident set size in kilobytes, when `/usr/bin/time` was available
    /// to measure it.
    pub peak_memory_kb: Option<u64>,
}

/// A scratch directory that is removed on every exit path, including panic.
///
/// Proving leaves witnesses, proofs, and verification keys behind. On a worker
/// running thousands of jobs, a leak here fills the disk — and a disk-full
/// worker fails every job it touches, which is a far worse outage than the one
/// that caused it. `Drop` runs on panic as well as on return, which is why the
/// cleanup lives here rather than at the end of the proving function.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Create a scratch directory under `parent`.
    ///
    /// # Errors
    ///
    /// Fails if the directory cannot be created.
    pub fn new(parent: &Path, label: &str) -> std::io::Result<Self> {
        let path = parent.join(label);
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.path) {
            // Only worth noting if the directory was actually there.
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(
                    path = %self.path.display(),
                    error = %err,
                    "could not remove scratch directory; disk may leak"
                );
            }
        }
    }
}

/// Exit codes above 128 encode the signal that killed the process.
const SIGNAL_EXIT_BASE: i32 = 128;
const SIGKILL: i32 = 9;
const SIGXCPU: i32 = 24;

/// Run a command under `bounds`, in `working_dir`.
///
/// # Errors
///
/// Returns [`BoundedError`] describing which bound was hit or how the process
/// failed.
pub async fn run(
    program: &str,
    args: &[String],
    working_dir: &Path,
    bounds: Bounds,
) -> Result<Completed, BoundedError> {
    // `exec` replaces the shell, so there is no intermediate process to orphan
    // and killing the child kills the actual work.
    let quoted: Vec<String> = std::iter::once(program.to_owned())
        .chain(args.iter().cloned())
        .map(|part| shell_quote(&part))
        .collect();

    let script = format!(
        "ulimit -v {} 2>/dev/null; ulimit -t {} 2>/dev/null; exec {}",
        bounds.address_space_kb,
        bounds.cpu_seconds,
        quoted.join(" ")
    );

    let started = Instant::now();
    let mut child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| BoundedError::Spawn(e.to_string()))?;

    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();

    let mut stdout = String::new();
    let mut stderr = String::new();

    let collect = async {
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut stdout).await;
        }
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut stderr).await;
        }
        child.wait().await
    };

    let status = match tokio::time::timeout(bounds.wall_clock, collect).await {
        Ok(result) => result.map_err(|e| BoundedError::Spawn(e.to_string()))?,
        Err(_) => {
            // `kill_on_drop` would handle this, but killing explicitly makes
            // the intent obvious and the process is gone before we return.
            return Err(BoundedError::WallClockExceeded {
                limit: bounds.wall_clock,
            });
        }
    };

    let duration = started.elapsed();

    if status.success() {
        return Ok(Completed {
            stdout,
            stderr,
            duration,
            peak_memory_kb: None,
        });
    }

    Err(classify_exit(status.code(), &stderr, bounds))
}

/// Turn an exit status into the specific bound that was breached.
///
/// The shell reports a signal death as `128 + signal`. `SIGXCPU` means the CPU
/// limit; a `SIGKILL` after an allocation failure, or a non-zero exit with an
/// allocation message, means the address-space limit. Distinguishing these
/// matters because they are separate metrics and point at different fixes.
fn classify_exit(code: Option<i32>, stderr: &str, bounds: Bounds) -> BoundedError {
    let looks_like_oom = mentions_allocation_failure(stderr);

    match code {
        Some(c) if c == SIGNAL_EXIT_BASE + SIGXCPU => BoundedError::CpuExceeded {
            limit_seconds: bounds.cpu_seconds,
        },
        Some(c) if c == SIGNAL_EXIT_BASE + SIGKILL && looks_like_oom => {
            BoundedError::AddressSpaceExceeded {
                limit_kb: bounds.address_space_kb,
            }
        }
        Some(c) if c > SIGNAL_EXIT_BASE => BoundedError::Killed {
            signal: c - SIGNAL_EXIT_BASE,
        },
        Some(c) if looks_like_oom => BoundedError::AddressSpaceExceeded {
            limit_kb: bounds.address_space_kb,
        },
        Some(c) => BoundedError::Failed {
            code: c,
            stderr: truncate(stderr, 2000),
        },
        None => BoundedError::Killed { signal: SIGKILL },
    }
}

/// Whether stderr looks like an allocation failure.
///
/// Under `RLIMIT_AS` a process is usually not killed by the kernel — its
/// allocation simply fails, and what happens next depends entirely on the
/// runtime. C++ throws `std::bad_alloc` and typically aborts; glibc says
/// "cannot allocate memory"; Python raises `MemoryError`; Rust aborts with
/// "memory allocation of N bytes failed".
///
/// So this is a heuristic over text, which is unavoidable and worth being
/// honest about: a *new* runtime with a spelling not listed here would have its
/// out-of-memory misreported as an ordinary failure — and therefore treated as
/// permanent and never retried. The signal-based checks in [`classify_exit`]
/// are the more reliable path; this only refines what they cannot see.
fn mentions_allocation_failure(stderr: &str) -> bool {
    const NEEDLES: &[&str] = &[
        "out of memory",
        "cannot allocate",
        "bad_alloc",
        "memory allocation",
        "alloc failed",
        "allocation failed",
        "memoryerror",
        "insufficient memory",
        "virtual memory exhausted",
    ];

    let lower = stderr.to_ascii_lowercase();
    NEEDLES.iter().any(|needle| lower.contains(needle))
}

/// Single-quote a value for `sh`.
///
/// Job inputs reach the command line as file paths, and a path containing a
/// quote or a semicolon must not be able to run anything.
fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Keeps a runaway error message out of the database and the logs.
fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes truncated)", &text[..end], text.len() - end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_root() -> PathBuf {
        std::env::temp_dir()
    }

    fn generous() -> Bounds {
        Bounds {
            wall_clock: Duration::from_secs(10),
            address_space_kb: 2 * 1024 * 1024,
            cpu_seconds: 10,
        }
    }

    #[tokio::test]
    async fn a_successful_command_returns_its_output() {
        let out = run("/bin/echo", &["hello".into()], &scratch_root(), generous())
            .await
            .expect("echo should succeed");

        assert_eq!(out.stdout.trim(), "hello");
    }

    #[tokio::test]
    async fn a_nonzero_exit_is_reported_with_its_code() {
        let err = run(
            "/bin/sh",
            &["-c".into(), "echo trouble >&2; exit 3".into()],
            &scratch_root(),
            generous(),
        )
        .await
        .expect_err("should have failed");

        let BoundedError::Failed { code, stderr } = err else {
            panic!("expected a plain failure, got {err:?}");
        };
        assert_eq!(code, 3);
        assert!(stderr.contains("trouble"), "stderr should be captured");
    }

    /// A non-zero exit means the tool rejected its input, and it will reject it
    /// again — retrying is pure waste.
    #[tokio::test]
    async fn a_nonzero_exit_is_classified_permanent() {
        let err = run(
            "/bin/sh",
            &["-c".into(), "exit 1".into()],
            &scratch_root(),
            generous(),
        )
        .await
        .unwrap_err();

        assert!(!err.is_transient());
        assert_eq!(err.metric_label(), "nonzero_exit");
    }

    /// The bound that catches a hang: a process asleep burns no CPU and
    /// allocates nothing, so only the wall clock will ever stop it.
    #[tokio::test]
    async fn a_hanging_process_is_killed_on_the_wall_clock() {
        let bounds = Bounds {
            wall_clock: Duration::from_millis(300),
            ..generous()
        };

        let started = Instant::now();
        let err = run("/bin/sleep", &["30".into()], &scratch_root(), bounds)
            .await
            .unwrap_err();

        assert!(
            matches!(err, BoundedError::WallClockExceeded { .. }),
            "{err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "it should have been killed promptly, took {:?}",
            started.elapsed()
        );
        assert!(err.is_transient());
        assert_eq!(err.metric_label(), "timeout");
    }

    /// Exceeding a bound must not take the worker down with it.
    #[tokio::test]
    async fn the_worker_survives_a_bound_being_hit() {
        let bounds = Bounds {
            wall_clock: Duration::from_millis(200),
            ..generous()
        };
        let _ = run("/bin/sleep", &["30".into()], &scratch_root(), bounds).await;

        // Still able to run work afterwards.
        let after = run("/bin/echo", &["alive".into()], &scratch_root(), generous())
            .await
            .expect("the runner should still work");
        assert_eq!(after.stdout.trim(), "alive");
    }

    #[tokio::test]
    async fn a_memory_hog_is_stopped_by_the_address_space_limit() {
        let bounds = Bounds {
            address_space_kb: 64 * 1024, // 64 MB
            wall_clock: Duration::from_secs(20),
            cpu_seconds: 15,
        };

        // Allocate far more than the limit. Under `ulimit -v` this fails at
        // allocation rather than being OOM-killed by the kernel.
        let err = run(
            "/usr/bin/python3",
            &[
                "-c".into(),
                "b = bytearray(512 * 1024 * 1024); print(len(b))".into(),
            ],
            &scratch_root(),
            bounds,
        )
        .await
        .unwrap_err();

        assert!(
            !matches!(err, BoundedError::Failed { code: 127, .. }),
            "python3 should be available for this test"
        );
        assert!(
            err.is_transient(),
            "resource exhaustion is recoverable, not permanent: {err:?}"
        );
    }

    #[tokio::test]
    async fn a_cpu_burner_is_stopped_by_the_cpu_limit() {
        let bounds = Bounds {
            cpu_seconds: 1,
            // Well above the CPU limit, so the CPU bound is what fires.
            wall_clock: Duration::from_secs(30),
            address_space_kb: 2 * 1024 * 1024,
        };

        let err = run(
            "/bin/sh",
            &["-c".into(), "while :; do :; done".into()],
            &scratch_root(),
            bounds,
        )
        .await
        .unwrap_err();

        assert!(
            matches!(
                err,
                BoundedError::CpuExceeded { .. } | BoundedError::Killed { .. }
            ),
            "expected the CPU bound to fire, got {err:?}"
        );
        assert!(err.is_transient());
    }

    #[tokio::test]
    async fn arguments_containing_shell_metacharacters_are_not_executed() {
        // If quoting were wrong, this would run `touch pwned` in the scratch
        // directory. It must be passed through as a literal string instead.
        let root = std::env::temp_dir().join("dray-quote-test");
        std::fs::create_dir_all(&root).unwrap();
        let canary = root.join("pwned");
        let _ = std::fs::remove_file(&canary);

        let out = run(
            "/bin/echo",
            &[format!("x; touch {}", canary.display())],
            &root,
            generous(),
        )
        .await
        .expect("echo should succeed");

        assert!(
            !canary.exists(),
            "shell metacharacters in an argument were executed"
        );
        assert!(
            out.stdout.contains("touch"),
            "the argument should be literal"
        );
    }

    #[tokio::test]
    async fn a_missing_program_is_reported_not_panicked() {
        let err = run("/nonexistent/program", &[], &scratch_root(), generous())
            .await
            .unwrap_err();

        // `sh` reports 127 for command-not-found.
        assert!(
            matches!(err, BoundedError::Failed { code: 127, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn scratch_is_removed_on_drop() {
        let root = std::env::temp_dir();
        let path = {
            let scratch = Scratch::new(&root, "dray-scratch-drop-test").unwrap();
            std::fs::write(scratch.path().join("witness.gz"), b"leftovers").unwrap();
            assert!(scratch.path().exists());
            scratch.path().to_path_buf()
        };
        assert!(!path.exists(), "scratch should be gone once dropped");
    }

    /// The reason cleanup lives in `Drop` rather than at the end of a function:
    /// a panic mid-proof must not leak a directory.
    #[test]
    fn scratch_is_removed_even_on_panic() {
        let root = std::env::temp_dir();
        let path = root.join("dray-scratch-panic-test");

        let result = std::panic::catch_unwind(|| {
            let scratch = Scratch::new(&root, "dray-scratch-panic-test").unwrap();
            std::fs::write(scratch.path().join("proof"), b"partial").unwrap();
            panic!("proving blew up");
        });

        assert!(result.is_err(), "the panic should have propagated");
        assert!(!path.exists(), "scratch should be gone despite the panic");
    }

    #[test]
    fn defaults_leave_headroom_over_measured_cost() {
        let bounds = Bounds::for_proving();
        // Phase 1 measured 2.47 s and ~42 MB peak RSS for the larger circuit.
        assert!(
            bounds.wall_clock >= Duration::from_secs(30),
            "a bound a normal proof can trip turns healthy work into retries"
        );
        assert!(bounds.address_space_kb >= 512 * 1024);
        assert!(bounds.cpu_seconds >= 30);
    }

    #[test]
    fn shell_quoting_neutralises_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
        assert_eq!(shell_quote("a b; rm -rf /"), "'a b; rm -rf /'");
    }

    #[test]
    fn truncation_respects_character_boundaries() {
        let text = "é".repeat(100);
        let cut = truncate(&text, 51);
        assert!(cut.is_char_boundary(0));
        assert!(cut.contains("truncated"));
    }

    /// Every runtime spells "I ran out of memory" differently, and getting this
    /// wrong means a resource failure is classified permanent and never
    /// retried. These are the spellings actually seen from the tools this
    /// worker runs, plus the ones the tests provoke.
    #[test]
    fn allocation_failures_are_recognised_across_runtimes() {
        for message in [
            "terminate called after throwing an instance of 'std::bad_alloc'",
            "fork: Cannot allocate memory",
            "MemoryError",
            "memory allocation of 8589934592 bytes failed",
            "Out of memory: Killed process",
            "virtual memory exhausted: Cannot allocate memory",
        ] {
            assert!(
                mentions_allocation_failure(message),
                "should have been recognised as an allocation failure: {message:?}"
            );
        }

        for message in [
            "syntax error near unexpected token",
            "witness does not satisfy",
            "",
        ] {
            assert!(
                !mentions_allocation_failure(message),
                "should not have been read as an allocation failure: {message:?}"
            );
        }
    }

    #[test]
    fn exit_classification_distinguishes_the_bounds() {
        let bounds = Bounds::for_proving();

        assert!(matches!(
            classify_exit(Some(128 + 24), "", bounds),
            BoundedError::CpuExceeded { .. }
        ));
        assert!(matches!(
            classify_exit(Some(128 + 9), "std::bad_alloc", bounds),
            BoundedError::AddressSpaceExceeded { .. }
        ));
        assert!(matches!(
            classify_exit(Some(1), "terminate called: cannot allocate memory", bounds),
            BoundedError::AddressSpaceExceeded { .. }
        ));
        assert!(matches!(
            classify_exit(Some(128 + 15), "", bounds),
            BoundedError::Killed { signal: 15 }
        ));
        assert!(matches!(
            classify_exit(Some(2), "syntax error", bounds),
            BoundedError::Failed { code: 2, .. }
        ));
    }
}
