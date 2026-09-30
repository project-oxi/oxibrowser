//! Cross-process advisory lock for account state (FM-L7 / M8b redesign).
//!
//! One `flock(2)`-exclusive lock file per account
//! (`~/.oxibrowser/accounts/<id>/lock`). The lock guards **envelope and
//! registry mutations only** — capture, state transitions, envelope
//! disposal — never page-session lifetimes, so a long-lived `serve` never
//! starves CLI processes.
//!
//! Properties:
//!
//! - **Crash-safe**: the kernel drops the lock when the holding process
//!   dies; stale lock files are inert content, never a deadlock.
//! - **Advisory**: only cooperating oxibrowser surfaces respect it. A
//!   non-cooperating local process is out of scope (same trust boundary as
//!   the audit log, design §9 FM-8).
//! - **Not reentrant**: acquiring twice from the same process via two
//!   file descriptions blocks. Callers must not nest acquisitions — the
//!   [`super::AccountManager`] entry points are structured so they never do.

use std::fs::{File, OpenOptions};
use std::path::Path;
use std::time::{Duration, Instant};

use crate::error::{CoreError, Result};

/// How to react when the lock is held elsewhere.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockPolicy {
    /// Fail immediately with [`CoreError::AccountLocked`] — the CLI default
    /// (concurrent runs surface loudly instead of queueing silently).
    FailFast,
    /// Block until the lock is free (`--lock-wait`).
    Block,
    /// Block up to a deadline, then fail (`--lock-timeout <SEC>`).
    Timeout(Duration),
}

#[allow(clippy::derivable_impls)]
impl Default for LockPolicy {
    fn default() -> Self {
        LockPolicy::FailFast
    }
}

impl LockPolicy {
    /// Human-readable mode name for error details.
    pub fn as_str(&self) -> &'static str {
        match self {
            LockPolicy::FailFast => "fail-fast",
            LockPolicy::Block => "block",
            LockPolicy::Timeout(_) => "timeout",
        }
    }
}

/// Held exclusive lock on one account. Dropping releases (fd close).
#[derive(Debug)]
pub struct AccountLockGuard {
    _file: File,
}
/// Acquire the account lock at `path` under `policy`.
///
/// The file is created (directory already 0700) if absent; its content is
/// never meaningful.
pub fn acquire(path: &Path, policy: &LockPolicy) -> Result<AccountLockGuard> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)
        .map_err(|e| {
            CoreError::SessionError(format!(
                "account: cannot open lock file {}: {e}",
                path.display()
            ))
        })?;
    let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
    let try_lock = || unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0;
    let deadline = match policy {
        LockPolicy::FailFast => None,
        LockPolicy::Block => None,
        LockPolicy::Timeout(t) => Some(Instant::now() + *t),
    };
    loop {
        if try_lock() {
            return Ok(AccountLockGuard { _file: file });
        }
        let err = std::io::Error::last_os_error();
        // LOCK_NB never sleeps; EINTR is still possible — retry, don't abort.
        if err.raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        let would_block = err.raw_os_error() == Some(libc::EWOULDBLOCK);
        if !would_block {
            return Err(CoreError::SessionError(format!(
                "account: flock on {} failed: {err}",
                path.display()
            )));
        }
        match (deadline, policy) {
            // FailFast: no retry.
            (None, LockPolicy::FailFast) => {
                return Err(CoreError::AccountLocked {
                    detail: format!("lock file {}", path.display()),
                });
            }
            // Block: retry forever (interruptible errors surface above).
            (None, LockPolicy::Block) => {}
            // Timeout always sets a deadline above.
            (None, LockPolicy::Timeout(_)) => unreachable!("timeout policy sets a deadline"),
            (Some(d), _) => {
                if Instant::now() >= d {
                    return Err(CoreError::AccountLocked {
                        detail: format!(
                            "lock timeout after {}s on {}",
                            policy_timeout_secs(policy),
                            path.display()
                        ),
                    });
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn policy_timeout_secs(policy: &LockPolicy) -> f64 {
    match policy {
        LockPolicy::Timeout(d) => d.as_secs_f64(),
        _ => 0.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_lock(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oxi-acct-lock-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("lock")
    }

    #[test]
    fn second_acquire_fails_fast_then_drop_releases() {
        let path = tmp_lock("fast");
        let _ = std::fs::remove_file(&path);
        let g = acquire(&path, &LockPolicy::FailFast).unwrap();
        // Same process, second file description: blocks → FailFast errors.
        assert!(matches!(
            acquire(&path, &LockPolicy::FailFast),
            Err(CoreError::AccountLocked { .. })
        ));
        drop(g);
        // Releasable immediately after.
        assert!(acquire(&path, &LockPolicy::FailFast).is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn timeout_expires_with_account_locked() {
        let path = tmp_lock("timeout");
        let _ = std::fs::remove_file(&path);
        let _g = acquire(&path, &LockPolicy::FailFast).unwrap();
        let start = Instant::now();
        let err = acquire(&path, &LockPolicy::Timeout(Duration::from_millis(150))).unwrap_err();
        assert!(matches!(err, CoreError::AccountLocked { .. }));
        assert!(start.elapsed() >= Duration::from_millis(100));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn released_lock_is_reacquirable_from_another_handle() {
        // Simulates cross-process release: guard dropped by "process A".
        let path = tmp_lock("handoff");
        let _ = std::fs::remove_file(&path);
        let g = acquire(&path, &LockPolicy::FailFast).unwrap();
        drop(g);
        let g2 = acquire(&path, &LockPolicy::Timeout(Duration::from_millis(200)));
        assert!(g2.is_ok());
        let _ = std::fs::remove_file(&path);
    }
}
