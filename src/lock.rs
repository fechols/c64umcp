//! Per-device ownership, shared by every server process on this computer.
//!
//! The in-process `gate` serializes one server's tools. Agents often run their
//! own server process each, so every device call also takes an OS file lock
//! named after the device's address. A server keeps that lock for an idle
//! lease after its last call (longer while `claim_device` holds it), so
//! another agent cannot slip in between one agent's related calls. A server
//! that finds the device taken waits briefly, then is denied with a message
//! naming the owner, read from an owner record next to the lock file. It
//! covers processes on this computer only; the OS frees the lock when its
//! holder exits, even after a crash.
use std::{
    fs::{File, TryLockError},
    future::Future,
    io::ErrorKind,
    net::IpAddr,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, OwnedMutexGuard};
use url::{Host, Url};

/// Default wait for another process to release the device before denying.
pub const DEFAULT_LOCK_TIMEOUT: Duration = Duration::from_secs(2);
/// Default time a server keeps the device after its last call.
pub const DEFAULT_LOCK_LEASE: Duration = Duration::from_secs(10);
/// Longest single claim_device hold.
pub const MAX_CLAIM: Duration = Duration::from_secs(600);
/// Every denial starts with this, so agents and tests can recognize it.
pub const BUSY_PREFIX: &str = "device busy:";
const FIRST_POLL: Duration = Duration::from_millis(25);
const MAX_POLL: Duration = Duration::from_millis(250);
/// Bound on resolving the device hostname to its address for the lock name.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(5);
/// Owner-record temporary files older than this are leftovers of a killed writer.
const STALE_TEMP_AGE: Duration = Duration::from_secs(60);

type Resolved = Pin<Box<dyn Future<Output = std::io::Result<Vec<IpAddr>>> + Send>>;
/// Resolves a hostname to its addresses; replaceable in tests.
type Resolver = fn(String) -> Resolved;

fn system_resolver(name: String) -> Resolved {
    Box::pin(async move {
        Ok(tokio::net::lookup_host((name.as_str(), 0))
            .await?
            .map(|address| address.ip())
            .collect())
    })
}

/// Held for one device operation, including any cleanup task it hands off.
/// Dropping it starts this server's idle lease on the device (or releases
/// the device at once without one), then releases the in-process gate.
pub struct OperationGuard {
    _lease: Option<LeaseHold>,
    _gate: OwnedMutexGuard<()>,
}

impl OperationGuard {
    /// A guard holding only the in-process gate; for callers and tests that
    /// serialize within one process.
    pub fn gate_only(gate: OwnedMutexGuard<()>) -> Self {
        Self {
            _lease: None,
            _gate: gate,
        }
    }
}

/// One in-flight call's share of the device lock.
struct LeaseHold {
    lock: Arc<DeviceLock>,
    /// Whether finishing starts a new idle lease; read-only calls only keep
    /// the lease (or claim) that was already running.
    extend: bool,
}

impl Drop for LeaseHold {
    fn drop(&mut self) {
        DeviceLock::finish_call(&self.lock, self.extend);
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Paths {
    lock: PathBuf,
    owner: PathBuf,
}

impl Paths {
    fn new(dir: &Path, stem: &str) -> Self {
        Self {
            lock: dir.join(format!("{stem}.lock")),
            owner: dir.join(format!("{stem}.owner.json")),
        }
    }
}

#[derive(Default)]
struct State {
    /// The locked file while this server owns the device.
    file: Option<File>,
    /// The lock and owner files of `file`.
    paths: Option<Paths>,
    /// Calls in flight that hold the device.
    active: usize,
    /// Bumped on every change, so a stale release timer does nothing.
    generation: u64,
    /// The current call, or the last one while idle.
    tool: Option<String>,
    /// When the current call started, or when the last one finished.
    since: Option<SystemTime>,
    claimed_until: Option<SystemTime>,
    lease_until: Option<SystemTime>,
    /// The lease that was running when the current call started, kept by a
    /// call that does not extend it.
    prior_until: Option<SystemTime>,
}

/// Who owns the device, as written next to the lock file. It is a separate
/// file because Windows makes the locked file's bytes unreadable to others.
#[derive(Serialize, Deserialize)]
struct Owner {
    pid: u32,
    client: Option<String>,
    tool: Option<String>,
    /// A call is in flight.
    busy: bool,
    since_ms: u64,
    /// While idle: when the lease (including any claim) runs out.
    until_ms: Option<u64>,
    claimed_until_ms: Option<u64>,
}

/// This server's ownership of one device, shared with other server processes
/// through a lock file in a common directory.
pub struct DeviceLock {
    dir: PathBuf,
    host: Host<String>,
    port: Option<u16>,
    /// The device as configured, for messages.
    label: String,
    /// The last address a hostname resolved to, used when a later lookup fails.
    last_key: std::sync::Mutex<Option<String>>,
    resolve: Resolver,
    wait: Duration,
    idle: Duration,
    client: std::sync::Mutex<Option<String>>,
    state: std::sync::Mutex<State>,
}

impl DeviceLock {
    /// A lock for the device at `base`, in `dir`. `wait` bounds waiting for
    /// another process; `idle` is the lease kept after each call.
    pub fn new(dir: &Path, base: &Url, wait: Duration, idle: Duration) -> Option<Arc<Self>> {
        Self::with_resolver(dir, base, wait, idle, system_resolver)
    }

    fn with_resolver(
        dir: &Path,
        base: &Url,
        wait: Duration,
        idle: Duration,
        resolve: Resolver,
    ) -> Option<Arc<Self>> {
        let host = base.host()?.to_owned();
        let mut label = base.host_str()?.to_owned();
        if let Some(port) = base.port() {
            label.push_str(&format!(":{port}"));
        }
        Some(Arc::new(Self {
            dir: dir.to_owned(),
            host,
            port: base.port(),
            label,
            last_key: std::sync::Mutex::default(),
            resolve,
            wait,
            idle,
            client: std::sync::Mutex::default(),
            state: std::sync::Mutex::default(),
        }))
    }

    /// Record the MCP client's name for the owner record, once.
    pub fn set_client(&self, client: impl FnOnce() -> String) {
        let mut slot = self.client.lock().unwrap();
        if slot.is_none() {
            *slot = Some(client());
        }
    }

    /// The lock and owner files, named after the device's address so a
    /// hostname and its IP share one lock. A hostname is resolved each time
    /// this server takes the device afresh (startup makes no network call);
    /// when that fails, the last address it resolved to is used, and with
    /// none the call fails rather than lock under a name other servers would
    /// not use.
    async fn resolve_paths(&self) -> Result<Paths> {
        let key = match &self.host {
            Host::Ipv4(ip) => ip.to_string(),
            Host::Ipv6(ip) => ip.to_string(),
            Host::Domain(name) => {
                let resolved = tokio::time::timeout(RESOLVE_TIMEOUT, (self.resolve)(name.clone()))
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .and_then(|ips| pick_key(&ips));
                let mut last = self.last_key.lock().unwrap();
                match resolved {
                    Some(key) => last.insert(key).clone(),
                    None => last.clone().with_context(|| {
                        format!(
                            "could not resolve {name} to name the device lock; nothing was sent to the device (configure the C64U by IP address to avoid this)"
                        )
                    })?,
                }
            }
        };
        Ok(Paths::new(&self.dir, &file_stem(&key, self.port)))
    }

    /// Take the in-process gate and the device, waiting up to `wait` (the
    /// configured wait if None) for another process to release it, then
    /// fail with a `device busy:` message naming the owner. A call with
    /// `extend` false (a read-only one) does not start or extend the idle
    /// lease when it finishes.
    ///
    /// The lock file is polled without blocking, and the gate is released
    /// between polls, so dropping the future stops waiting at once and no
    /// thread is left blocked on the lock.
    pub async fn acquire(
        self: &Arc<Self>,
        gate: &Arc<Mutex<()>>,
        tool: Option<&str>,
        wait: Option<Duration>,
        extend: bool,
    ) -> Result<OperationGuard> {
        let wait = wait.unwrap_or(self.wait);
        let deadline = tokio::time::Instant::now() + wait;
        let mut poll = FIRST_POLL;
        // Opened only when this server does not already hold the device.
        let mut candidate: Option<(Paths, File)> = None;
        loop {
            if candidate.is_none() && self.state.lock().unwrap().file.is_none() {
                let paths = self.resolve_paths().await?;
                let path = paths.lock.clone();
                let file = tokio::task::spawn_blocking(move || open_lock_file(&path))
                    .await
                    .context("device lock task failed")?
                    .with_context(|| {
                        format!("could not open the device lock {}", paths.lock.display())
                    })?;
                candidate = Some((paths, file));
            }
            let gate = gate.clone().lock_owned().await;
            let mut fresh = None;
            {
                let mut state = self.state.lock().unwrap();
                let reused = state.file.is_some();
                if !reused {
                    // This server's lease ended after the check above: open a
                    // candidate first.
                    let Some((paths, file)) = &candidate else {
                        continue;
                    };
                    match file.try_lock() {
                        Ok(()) => {
                            let (paths, file) = candidate.take().expect("candidate checked above");
                            state.file = Some(file);
                            state.paths = Some(paths.clone());
                            fresh = Some(paths);
                        }
                        Err(TryLockError::WouldBlock) => {}
                        Err(TryLockError::Error(error)) => {
                            return Err(error).with_context(|| {
                                format!("could not take the device lock {}", paths.lock.display())
                            });
                        }
                    }
                }
                if state.file.is_some() {
                    // A read-only call keeps the lease that was running.
                    state.prior_until = if reused { state.lease_until } else { None };
                    state.active += 1;
                    state.generation += 1;
                    state.tool = tool.map(str::to_owned);
                    state.since = Some(SystemTime::now());
                    state.lease_until = None;
                    self.write_owner(&state);
                    drop(state);
                    if let Some(paths) = fresh {
                        // Holding the lock, no live writer for this device is
                        // mid-write: remove a killed writer's temporary files.
                        tokio::task::spawn_blocking(move || sweep_stale_temps(&paths));
                    }
                    return Ok(OperationGuard {
                        _lease: Some(LeaseHold {
                            lock: self.clone(),
                            extend,
                        }),
                        _gate: gate,
                    });
                }
            }
            drop(gate);
            let now = tokio::time::Instant::now();
            if now >= deadline {
                let owner = candidate.as_ref().map(|(paths, _)| paths.owner.as_path());
                return Err(anyhow::anyhow!(self.busy_message(owner, wait)));
            }
            tokio::time::sleep(poll.min(deadline - now)).await;
            poll = (poll * 2).min(MAX_POLL);
        }
    }

    /// A call finished: keep the device for the idle lease (or until a
    /// longer claim ends), then release it unless another call came first.
    /// A call that does not extend keeps only the lease it found.
    fn finish_call(self: &Arc<Self>, extend: bool) {
        let mut state = self.state.lock().unwrap();
        state.active -= 1;
        if state.active > 0 {
            return;
        }
        state.generation += 1;
        let now = SystemTime::now();
        let lease = if extend {
            now + self.idle
        } else {
            state.prior_until.unwrap_or(now)
        };
        let until = lease.max(state.claimed_until.unwrap_or(now));
        state.prior_until = None;
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return self.release_locked(&mut state);
        };
        if until <= now {
            return self.release_locked(&mut state);
        }
        state.since = Some(now);
        state.lease_until = Some(until);
        self.write_owner(&state);
        let generation = state.generation;
        let delay = until.duration_since(now).unwrap_or_default();
        let lock = self.clone();
        runtime.spawn(async move {
            tokio::time::sleep(delay).await;
            let mut state = lock.state.lock().unwrap();
            if state.generation == generation && state.active == 0 {
                lock.release_locked(&mut state);
            }
        });
    }

    /// Release the device now if no call holds it. Deletes the owner record
    /// before unlocking, so it never removes the next owner's record.
    fn release_locked(&self, state: &mut State) {
        if state.active > 0 {
            return;
        }
        state.generation += 1;
        state.claimed_until = None;
        state.lease_until = None;
        state.prior_until = None;
        state.tool = None;
        let paths = state.paths.take();
        if let Some(file) = state.file.take() {
            if let Some(paths) = paths {
                let _ = std::fs::remove_file(&paths.owner);
            }
            let _ = file.unlock();
        }
    }

    /// Hold the device for `duration` from now, across calls, replacing any
    /// earlier claim. Returns when the claim ends.
    pub async fn claim(
        self: &Arc<Self>,
        gate: &Arc<Mutex<()>>,
        duration: Duration,
    ) -> Result<SystemTime> {
        let guard = self.acquire(gate, Some("claim_device"), None, true).await?;
        let until = SystemTime::now() + duration;
        self.state.lock().unwrap().claimed_until = Some(until);
        // Dropping the guard starts the lease, which now lasts until the claim ends.
        drop(guard);
        Ok(until)
    }

    /// Give up the device now, after this server's calls (and the cleanup
    /// they hand off) finish. Returns whether this server held it.
    pub async fn release(&self, gate: &Arc<Mutex<()>>) -> bool {
        let _gate = gate.lock().await;
        self.release_now()
    }

    /// Give up the device now unless a call is in flight; for shutdown,
    /// after in-flight calls were joined. Returns whether it was held.
    pub fn release_now(&self) -> bool {
        let mut state = self.state.lock().unwrap();
        let held = state.file.is_some() && state.active == 0;
        self.release_locked(&mut state);
        held
    }

    /// Whether this server holds the device, and who does if another server
    /// does. Checking another owner briefly tries the lock, so a server
    /// configured with no wait could be denied in that instant.
    pub async fn status(&self) -> Result<Value> {
        let now = SystemTime::now();
        {
            let state = self.state.lock().unwrap();
            if let (Some(_), Some(paths)) = (&state.file, &state.paths) {
                return Ok(json!({
                    "device": self.label,
                    "lock_file": paths.lock.display().to_string(),
                    "held_by_this_server": true,
                    "call_in_flight": state.active > 0,
                    "lease_seconds_left": state.lease_until.map(|until| seconds_left(now, until)),
                    "claim_seconds_left": state.claimed_until.map(|until| seconds_left(now, until)),
                }));
            }
        }
        let paths = self.resolve_paths().await?;
        let path = paths.lock.clone();
        let other = tokio::task::spawn_blocking(move || -> std::io::Result<bool> {
            let file = open_lock_file(&path)?;
            match file.try_lock() {
                Ok(()) => {
                    file.unlock()?;
                    Ok(false)
                }
                Err(TryLockError::WouldBlock) => Ok(true),
                Err(TryLockError::Error(error)) => Err(error),
            }
        })
        .await
        .context("device lock task failed")?
        .context("could not check the device lock")?;
        Ok(json!({
            "device": self.label,
            "lock_file": paths.lock.display().to_string(),
            "held_by_this_server": false,
            "held_by_another_server": other,
            "owner": other.then(|| read_owner(&paths.owner).map(|owner| describe(&owner))).flatten(),
        }))
    }

    fn write_owner(&self, state: &State) {
        let Some(paths) = &state.paths else {
            return;
        };
        let owner = Owner {
            pid: std::process::id(),
            client: self.client.lock().unwrap().clone(),
            tool: state.tool.clone(),
            busy: state.active > 0,
            since_ms: state.since.map_or(0, millis),
            until_ms: state.lease_until.map(millis),
            claimed_until_ms: state.claimed_until.map(millis),
        };
        // Write a temporary file and rename it over the record, so readers
        // never see a partial one. Best effort: the lock itself is the authority.
        let temp = paths
            .owner
            .with_extension(format!("{}.tmp", std::process::id()));
        let written = serde_json::to_vec(&owner)
            .map_err(std::io::Error::other)
            .and_then(|bytes| std::fs::write(&temp, bytes))
            .and_then(|()| std::fs::rename(&temp, &paths.owner));
        if let Err(error) = written {
            let _ = std::fs::remove_file(&temp);
            tracing::debug!("could not write the device owner record: {error}");
        }
    }

    fn busy_message(&self, owner: Option<&Path>, waited: Duration) -> String {
        let owner = match owner.and_then(read_owner) {
            Some(owner) => describe(&owner),
            None => "no owner details available".into(),
        };
        format!(
            "{BUSY_PREFIX} C64U {} is in use by another MCP server ({owner}). Request denied after waiting {:.1}s; nothing was sent to the device. Retry later, or ask the agent using it to call release_device.",
            self.label,
            waited.as_secs_f32(),
        )
    }
}

fn read_owner(path: &Path) -> Option<Owner> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// Remove owner-record temporary files (`<stem>.owner.<pid>.tmp`) that a
/// killed writer left behind. Best effort.
fn sweep_stale_temps(paths: &Paths) {
    let (Some(dir), Some(name)) = (paths.owner.parent(), paths.owner.file_name()) else {
        return;
    };
    // `<stem>.owner.json` gives the prefix `<stem>.owner.`
    let name = name.to_string_lossy();
    let Some(prefix) = name.strip_suffix("json") else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !(file_name.starts_with(prefix) && file_name.ends_with(".tmp")) {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| {
                now.duration_since(modified)
                    .is_ok_and(|age| age >= STALE_TEMP_AGE)
            });
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// A one-line description of the device's owner, relative to now.
fn describe(owner: &Owner) -> String {
    let now = millis(SystemTime::now());
    let seconds = |ms: u64| ms as f64 / 1000.0;
    let mut text = format!("pid {}", owner.pid);
    if let Some(client) = &owner.client {
        text.push_str(&format!(", client \"{client}\""));
    }
    let tool = owner.tool.as_deref().unwrap_or("a tool call");
    let claimed = owner.claimed_until_ms.filter(|&until| until > now);
    if owner.busy {
        text.push_str(&format!(
            ", running {tool} for {:.1}s",
            seconds(now.saturating_sub(owner.since_ms))
        ));
        if let Some(until) = claimed {
            text.push_str(&format!("; claimed for {:.0}s more", seconds(until - now)));
        }
    } else if let Some(until) = claimed {
        text.push_str(&format!(
            ", claimed with claim_device for {:.0}s more",
            seconds(until - now)
        ));
    } else if let Some(until) = owner.until_ms.filter(|&until| until > now) {
        text.push_str(&format!(
            ", finished {tool} {:.1}s ago and keeps the device for {:.1}s more (idle lease)",
            seconds(now.saturating_sub(owner.since_ms)),
            seconds(until - now)
        ));
    } else {
        text.push_str(", releasing it");
    }
    text
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

fn seconds_left(now: SystemTime, until: SystemTime) -> f64 {
    let left = until.duration_since(now).unwrap_or_default().as_secs_f64();
    (left * 10.0).round() / 10.0
}

/// The address a hostname's lock is named after: its first IPv4 address,
/// else its first address.
fn pick_key(ips: &[IpAddr]) -> Option<String> {
    ips.iter()
        .find(|ip| ip.is_ipv4())
        .or(ips.first())
        .map(ToString::to_string)
}

/// `c64umcp-<address>[-<port>]`, with unsafe characters replaced.
fn file_stem(key: &str, port: Option<u16>) -> String {
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
    push(key);
    if let Some(port) = port {
        push(&format!("-{port}"));
    }
    name
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn lock_names_follow_address_and_explicit_port() {
        assert_eq!(file_stem("192.168.66.178", None), "c64umcp-192.168.66.178");
        assert_eq!(file_stem("127.0.0.1", Some(8080)), "c64umcp-127.0.0.1-8080");
        assert_eq!(file_stem("::1", None), "c64umcp-__1");
    }

    #[test]
    fn a_hostname_is_named_after_its_ipv4_address() {
        let v4: IpAddr = "192.168.66.178".parse().unwrap();
        let v6: IpAddr = "fe80::1".parse().unwrap();
        assert_eq!(pick_key(&[v6, v4]).as_deref(), Some("192.168.66.178"));
        assert_eq!(pick_key(&[v6]).as_deref(), Some("fe80::1"));
        assert_eq!(pick_key(&[]), None);
    }

    fn lock_for(url: &str, resolve: Resolver) -> Arc<DeviceLock> {
        DeviceLock::with_resolver(
            Path::new("locks"),
            &Url::parse(url).unwrap(),
            Duration::ZERO,
            Duration::ZERO,
            resolve,
        )
        .unwrap()
    }

    fn unreachable_resolver(_: String) -> Resolved {
        panic!("an IP address needs no lookup")
    }

    #[tokio::test]
    async fn an_address_names_its_lock_without_a_lookup() {
        let lock = |url| lock_for(url, unreachable_resolver);
        let paths = lock("http://127.0.0.1:8080/v1/")
            .resolve_paths()
            .await
            .unwrap();
        assert_eq!(paths.lock, Path::new("locks/c64umcp-127.0.0.1-8080.lock"));
        assert_eq!(
            paths.owner,
            Path::new("locks/c64umcp-127.0.0.1-8080.owner.json")
        );
        let paths = lock("http://[::1]/v1/").resolve_paths().await.unwrap();
        assert_eq!(paths.lock, Path::new("locks/c64umcp-__1.lock"));
    }

    static RESOLVES: AtomicBool = AtomicBool::new(true);

    /// Resolves to 192.0.2.7 while `RESOLVES` is set, else fails.
    fn flaky_resolver(_: String) -> Resolved {
        let resolves = RESOLVES.load(Ordering::SeqCst);
        Box::pin(async move {
            if resolves {
                Ok(vec!["192.0.2.7".parse().unwrap()])
            } else {
                Err(std::io::Error::other("no such host"))
            }
        })
    }

    #[tokio::test]
    async fn a_failed_lookup_reuses_the_last_address_and_never_the_name() {
        RESOLVES.store(false, Ordering::SeqCst);
        let lock = lock_for("http://c64u.example/v1/", flaky_resolver);
        let error = lock.resolve_paths().await.unwrap_err().to_string();
        assert!(
            error.contains("could not resolve c64u.example") && error.contains("nothing was sent"),
            "{error}"
        );
        RESOLVES.store(true, Ordering::SeqCst);
        let resolved = lock.resolve_paths().await.unwrap();
        assert_eq!(resolved.lock, Path::new("locks/c64umcp-192.0.2.7.lock"));
        RESOLVES.store(false, Ordering::SeqCst);
        assert_eq!(lock.resolve_paths().await.unwrap(), resolved);
    }

    const DEVICE: &str = "http://192.0.2.64/v1/";

    /// A lock for a fresh, test-specific directory.
    fn temp_lock(name: &str, wait: Duration, idle: Duration) -> Arc<DeviceLock> {
        let dir =
            std::env::temp_dir().join(format!("c64umcp-lock-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        DeviceLock::new(&dir, &Url::parse(DEVICE).unwrap(), wait, idle).unwrap()
    }

    /// Another server process's view of the same device.
    fn other(lock: &DeviceLock, wait: Duration, idle: Duration) -> Arc<DeviceLock> {
        DeviceLock::new(&lock.dir, &Url::parse(DEVICE).unwrap(), wait, idle).unwrap()
    }

    fn paths(lock: &DeviceLock) -> Paths {
        Paths::new(&lock.dir, &file_stem("192.0.2.64", None))
    }

    async fn take(lock: &Arc<DeviceLock>, tool: &str) -> Result<OperationGuard> {
        lock.acquire(&Arc::default(), Some(tool), None, true).await
    }

    async fn read(lock: &Arc<DeviceLock>, tool: &str) -> Result<OperationGuard> {
        lock.acquire(&Arc::default(), Some(tool), None, false).await
    }

    #[tokio::test]
    async fn a_busy_device_is_denied_with_the_owners_details() {
        let a = temp_lock("busy", Duration::ZERO, Duration::ZERO);
        a.set_client(|| "agent-a 1.0".into());
        let held = take(&a, "capture_screenshot").await.unwrap();
        let b = other(&a, Duration::from_millis(100), Duration::ZERO);
        let started = std::time::Instant::now();
        let error = take(&b, "get_info").await.err().unwrap().to_string();
        assert!(started.elapsed() >= Duration::from_millis(100));
        assert!(error.starts_with(BUSY_PREFIX), "{error}");
        for part in [
            "C64U 192.0.2.64",
            &format!("pid {}", std::process::id()),
            "client \"agent-a 1.0\"",
            "running capture_screenshot",
            "nothing was sent",
        ] {
            assert!(error.contains(part), "{part} missing: {error}");
        }
        drop(held);
        // Released at once without a lease, and its record removed.
        assert!(
            !paths(&a).owner.exists(),
            "the owner record outlived the release"
        );
        take(&b, "get_info").await.unwrap();
    }

    #[tokio::test]
    async fn no_wait_tries_once() {
        let a = temp_lock("nowait", Duration::ZERO, Duration::ZERO);
        let _held = take(&a, "reset_machine").await.unwrap();
        let b = other(&a, Duration::ZERO, Duration::ZERO);
        let started = std::time::Instant::now();
        assert!(take(&b, "get_info").await.is_err());
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn the_idle_lease_keeps_the_device_then_lets_it_go() {
        let a = temp_lock("lease", Duration::ZERO, Duration::from_millis(400));
        drop(take(&a, "run_prg").await.unwrap());
        let b = other(&a, Duration::ZERO, Duration::ZERO);
        let error = take(&b, "get_info").await.err().unwrap().to_string();
        assert!(
            error.contains("finished run_prg") && error.contains("idle lease"),
            "{error}"
        );
        // The owner itself keeps working during its lease.
        drop(take(&a, "type_text").await.unwrap());
        tokio::time::sleep(Duration::from_millis(700)).await;
        take(&b, "get_info").await.unwrap();
    }

    #[tokio::test]
    async fn a_read_neither_starts_nor_extends_the_lease() {
        let a = temp_lock("read", Duration::ZERO, Duration::from_millis(600));
        let b = other(&a, Duration::ZERO, Duration::ZERO);
        // Without a lease, a read releases the device when it finishes.
        drop(read(&a, "read_text_screen").await.unwrap());
        drop(take(&b, "get_info").await.unwrap());
        // During a lease, a read keeps its original end.
        drop(take(&a, "run_prg").await.unwrap());
        tokio::time::sleep(Duration::from_millis(400)).await;
        drop(read(&a, "read_text_screen").await.unwrap());
        assert!(take(&b, "get_info").await.is_err(), "the lease ended early");
        tokio::time::sleep(Duration::from_millis(500)).await;
        take(&b, "get_info")
            .await
            .expect("the read extended the lease");
    }

    #[tokio::test]
    async fn a_claim_holds_the_device_until_released() {
        let a = temp_lock("claim", Duration::ZERO, Duration::ZERO);
        let gate = Arc::default();
        a.claim(&gate, Duration::from_secs(60)).await.unwrap();
        let b = other(&a, Duration::ZERO, Duration::ZERO);
        let error = take(&b, "get_info").await.err().unwrap().to_string();
        assert!(error.contains("claimed with claim_device"), "{error}");
        // A read during the claim keeps it.
        drop(read(&a, "read_memory").await.unwrap());
        assert!(take(&b, "get_info").await.is_err());
        assert_eq!(a.status().await.unwrap()["held_by_this_server"], true);
        let seen = b.status().await.unwrap();
        assert_eq!(seen["held_by_another_server"], true, "{seen}");
        assert!(a.release(&gate).await);
        assert!(
            !paths(&a).owner.exists(),
            "the owner record outlived the release"
        );
        take(&b, "get_info").await.unwrap();
    }

    #[tokio::test]
    async fn waiting_times_out_and_leaves_the_gate_free() {
        let a = temp_lock("timeout", Duration::ZERO, Duration::ZERO);
        let _held = take(&a, "upload_reu").await.unwrap();
        let b = other(&a, Duration::from_millis(600), Duration::ZERO);
        let gate = Arc::new(Mutex::new(()));
        let waiting = {
            let gate = gate.clone();
            tokio::spawn(async move { b.acquire(&gate, None, None, true).await })
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
        assert!(error.to_string().starts_with(BUSY_PREFIX), "{error:#}");
    }

    #[tokio::test]
    async fn a_dropped_wait_leaves_nothing_behind() {
        let a = temp_lock("dropped", Duration::ZERO, Duration::ZERO);
        let holder = take(&a, "upload_reu").await.unwrap();
        let b = other(&a, Duration::from_secs(30), Duration::ZERO);
        let gate = Arc::new(Mutex::new(()));
        let wait = b.acquire(&gate, None, None, true);
        assert!(
            tokio::time::timeout(Duration::from_millis(100), wait)
                .await
                .is_err()
        );
        drop(holder);
        // The abandoned wait holds neither lock.
        tokio::time::timeout(Duration::from_secs(1), b.acquire(&gate, None, None, true))
            .await
            .expect("the lock should be free after the wait was dropped")
            .unwrap();
    }

    #[tokio::test]
    async fn a_read_only_lock_file_can_be_locked() {
        let a = temp_lock("readonly", Duration::ZERO, Duration::ZERO);
        let path = paths(&a).lock;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"").unwrap();
        let set_readonly = |readonly: bool| {
            let mut permissions = std::fs::metadata(&path).unwrap().permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(readonly);
            std::fs::set_permissions(&path, permissions).unwrap();
        };
        set_readonly(true);
        let guard = take(&a, "get_info").await;
        set_readonly(false);
        guard.unwrap();
    }

    #[test]
    fn stale_owner_temp_files_are_swept() {
        let dir = std::env::temp_dir().join(format!("c64umcp-sweep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = Paths::new(&dir, "c64umcp-192.0.2.64");
        let stale = dir.join("c64umcp-192.0.2.64.owner.999.tmp");
        let recent = dir.join("c64umcp-192.0.2.64.owner.998.tmp");
        let unrelated = dir.join("c64umcp-192.0.2.65.owner.997.tmp");
        for path in [&stale, &recent, &unrelated] {
            std::fs::write(path, b"{}").unwrap();
        }
        let old = SystemTime::now() - Duration::from_secs(120);
        for path in [&stale, &unrelated] {
            File::options()
                .write(true)
                .open(path)
                .unwrap()
                .set_modified(old)
                .unwrap();
        }
        sweep_stale_temps(&paths);
        assert!(!stale.exists());
        assert!(recent.exists() && unrelated.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
