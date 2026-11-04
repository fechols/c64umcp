//! Per-device operation lock, shared by every server process on this computer.
//!
//! The in-process `gate` serializes one server's tools. Agents often run their
//! own server process each, so multi-step operations (pause/resume, uploads,
//! resets, video capture, input) also take an OS file lock named after the
//! device host. It covers processes on this computer only.
use std::{
    fs::{File, TryLockError},
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::sync::{Mutex, OwnedMutexGuard};
use url::Url;

/// Default bound on waiting for another process's device lock.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(120);
const FIRST_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_millis(250);

/// Held for one device operation, including any cleanup task it hands off.
/// Dropping it releases the cross-process lock, then the in-process gate.
pub struct OperationGuard {
    _device: Option<File>,
    _gate: OwnedMutexGuard<()>,
}

impl OperationGuard {
    /// A guard holding only the in-process gate; for callers and tests that
    /// serialize within one process.
    pub fn gate_only(gate: OwnedMutexGuard<()>) -> Self {
        Self {
            _device: None,
            _gate: gate,
        }
    }
}

/// The lock file for a device: `<dir>/c64umcp-<host>[-<port>].lock`.
pub fn lock_path(dir: &Path, base: &Url) -> Option<PathBuf> {
    let host = base.host_str()?;
    let mut name = String::from("c64umcp-");
    let mut push = |text: &str| {
        name.extend(text.chars().map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                c
            } else {
                '_'
            }
        }))
    };
    push(host);
    if let Some(port) = base.port() {
        push(&format!("-{port}"));
    }
    name.push_str(".lock");
    Some(dir.join(name))
}

/// Open the lock file read-only, creating it if missing. Locking needs no
/// write access, so a file another user created (0644 in a shared temp
/// directory) still works.
fn open_lock_file(path: &Path) -> std::io::Result<File> {
    match File::open(path) {
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        opened => return opened,
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut create = File::options();
    create.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut create, 0o666);
    match create.open(path) {
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    File::open(path)
}

/// Take the in-process gate, then the device's file lock if one is configured.
///
/// The file lock is polled without blocking, and the gate is released between
/// polls, so this server's gate-only requests keep running while another
/// process holds the device. Dropping the future stops waiting at once; no
/// thread is left blocked on the lock. Fails after `timeout`.
pub async fn acquire(
    gate: &Arc<Mutex<()>>,
    path: Option<&Path>,
    timeout: Duration,
) -> Result<OperationGuard> {
    let Some(path) = path else {
        return Ok(OperationGuard::gate_only(gate.clone().lock_owned().await));
    };
    let display = path.display().to_string();
    let owned = path.to_owned();
    let file = tokio::task::spawn_blocking(move || open_lock_file(&owned))
        .await
        .context("device lock task failed")?
        .with_context(|| format!("could not open the device lock {display}"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    let mut poll = FIRST_POLL;
    loop {
        let gate = gate.clone().lock_owned().await;
        match file.try_lock() {
            Ok(()) => {
                return Ok(OperationGuard {
                    _device: Some(file),
                    _gate: gate,
                });
            }
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(error)) => {
                return Err(error)
                    .with_context(|| format!("could not take the device lock {display}"));
            }
        }
        drop(gate);
        let now = tokio::time::Instant::now();
        if now >= deadline {
            bail!(
                "another c64umcp process has held the device lock {display} for over {:.1}s",
                timeout.as_secs_f32()
            );
        }
        tokio::time::sleep(poll.min(deadline - now)).await;
        poll = (poll * 2).min(MAX_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_names_follow_host_and_explicit_port() {
        let dir = Path::new("locks");
        let url = |text: &str| Url::parse(text).unwrap();
        assert_eq!(
            lock_path(dir, &url("http://192.168.66.178/v1/")).unwrap(),
            dir.join("c64umcp-192.168.66.178.lock")
        );
        assert_eq!(
            lock_path(dir, &url("http://127.0.0.1:8080/v1/")).unwrap(),
            dir.join("c64umcp-127.0.0.1-8080.lock")
        );
        assert_eq!(
            lock_path(dir, &url("http://[::1]/v1/")).unwrap(),
            dir.join("c64umcp-___1_.lock")
        );
    }

    fn temp_lock(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("c64umcp-lock-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir.join("device.lock")
    }

    #[tokio::test]
    async fn waiting_times_out_and_leaves_the_gate_free() {
        let path = temp_lock("timeout");
        let _held = acquire(&Arc::default(), Some(&path), Duration::from_secs(1))
            .await
            .unwrap();
        let gate = Arc::new(Mutex::new(()));
        let waiting = {
            let gate = gate.clone();
            let path = path.clone();
            tokio::spawn(
                async move { acquire(&gate, Some(&path), Duration::from_millis(600)).await },
            )
        };
        tokio::time::sleep(Duration::from_millis(200)).await;
        // Between polls the waiter does not hold this process's gate.
        let mut free = false;
        for _ in 0..50 {
            if gate.try_lock().is_ok() {
                free = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(free, "the gate stayed held while waiting for the file lock");
        let error = waiting
            .await
            .unwrap()
            .err()
            .expect("the wait should time out");
        assert!(
            error.to_string().contains("another c64umcp process"),
            "{error:#}"
        );
    }

    #[tokio::test]
    async fn a_dropped_wait_leaves_nothing_behind() {
        let path = temp_lock("dropped");
        let holder = acquire(&Arc::default(), Some(&path), Duration::from_secs(1))
            .await
            .unwrap();
        let gate = Arc::new(Mutex::new(()));
        let wait = acquire(&gate, Some(&path), Duration::from_secs(30));
        assert!(
            tokio::time::timeout(Duration::from_millis(100), wait)
                .await
                .is_err()
        );
        drop(holder);
        // The abandoned wait holds neither lock.
        tokio::time::timeout(
            Duration::from_secs(1),
            acquire(&gate, Some(&path), Duration::from_secs(1)),
        )
        .await
        .expect("the lock should be free after the wait was dropped")
        .unwrap();
    }

    #[tokio::test]
    async fn a_read_only_lock_file_can_be_locked() {
        let path = temp_lock("readonly");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let set_readonly = |readonly: bool| {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(readonly);
            std::fs::set_permissions(&path, permissions).unwrap();
        };
        set_readonly(true);
        let guard = acquire(&Arc::default(), Some(&path), Duration::from_secs(1)).await;
        set_readonly(false);
        guard.unwrap();
    }
}
