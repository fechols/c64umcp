//! Per-device operation lock, shared by every server process on this computer.
//!
//! The in-process `gate` serializes one server's tools. Agents often run their
//! own server process each, so multi-step operations (pause/resume, uploads,
//! resets, video capture, paced typing) also take an OS file lock named after
//! the device host. It covers processes on this computer only.
use std::{
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use tokio::sync::{Mutex, OwnedMutexGuard};
use url::Url;

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

/// Take the in-process gate, then the device's file lock if one is configured.
/// The file lock is taken on a blocking thread so waiting for another process
/// never stalls the runtime. If this future is dropped while waiting, the
/// blocking thread releases the lock as soon as it gets it.
pub async fn acquire(gate: &Arc<Mutex<()>>, path: Option<&Path>) -> Result<OperationGuard> {
    let gate = gate.clone().lock_owned().await;
    let device = match path {
        None => None,
        Some(path) => {
            let path = path.to_owned();
            let display = path.display().to_string();
            let file = tokio::task::spawn_blocking(move || -> std::io::Result<File> {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let file = File::options()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&path)?;
                file.lock()?;
                Ok(file)
            })
            .await
            .context("device lock task failed")?
            .with_context(|| format!("could not take the device lock {display}"))?;
            Some(file)
        }
    };
    Ok(OperationGuard {
        _device: device,
        _gate: gate,
    })
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
}
