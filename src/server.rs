//! Typed MCP tools and shared per-device serialization.
use crate::{
    device::{Device, MAX_BODY_BYTES as MAX_FILE, REU_CHUNK, REU_SIZE},
    input,
    lock::{self, OperationGuard},
    screen, video,
};
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use reqwest::Method;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ContentBlock as Content, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    net::Ipv4Addr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{Mutex, RwLock},
};

/// Upper bound on joining in-flight handlers during shutdown. Cancelled
/// handlers release the dispatch lock promptly; this only limits a handler
/// that rmcp failed to cancel, so the process can still exit.
const SHUTDOWN_JOIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Bound on resuming the CPU after a cancelled REU upload paused it.
const RESUME_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
#[derive(Clone)]
pub struct C64Server {
    device: Device,
    gate: Arc<Mutex<()>>,
    /// Cross-process lock file for this device; None disables it.
    device_lock: Option<Arc<PathBuf>>,
    /// Destination of the video stream this server started with start_stream;
    /// a screenshot sends video back there instead of stopping it.
    video_stream: Arc<std::sync::Mutex<Option<String>>>,
    /// Inputs this server pressed with send_input and has not released; the
    /// server releases exactly these on shutdown.
    held: Arc<std::sync::Mutex<HashSet<input::HeldInput>>>,
    accepting_calls: Arc<RwLock<bool>>,
    capture_ip: Option<Ipv4Addr>,
    capture_port: u16,
    capture_timeout: Duration,
    tool_router: ToolRouter<Self>,
}
impl C64Server {
    pub fn new(
        device: Device,
        capture_ip: Option<Ipv4Addr>,
        capture_port: u16,
        capture_timeout: Duration,
    ) -> Self {
        Self {
            device,
            gate: Arc::new(Mutex::new(())),
            device_lock: None,
            video_stream: Arc::default(),
            held: Arc::default(),
            accepting_calls: Arc::new(RwLock::new(true)),
            capture_ip,
            capture_port,
            capture_timeout,
            tool_router: Self::tool_router(),
        }
    }

    /// Serialize multi-step and destructive operations with other server
    /// processes on this computer through a lock file in `dir`, named after
    /// the device host. Without a configured host there is nothing to lock.
    pub fn with_lock_dir(mut self, dir: impl AsRef<Path>) -> Self {
        self.device_lock = self
            .device
            .base_url()
            .and_then(|base| lock::lock_path(dir.as_ref(), base))
            .map(Arc::new);
        self
    }

    /// Take the in-process gate, then the cross-process device lock.
    async fn operation(&self) -> Result<OperationGuard> {
        lock::acquire(
            &self.gate,
            self.device_lock.as_deref().map(PathBuf::as_path),
        )
        .await
    }

    /// Finish device cleanup after the MCP service has stopped and cancelled
    /// its requests. Call this before shutting down the Tokio runtime, including
    /// when waiting for the service returned an error.
    pub async fn finish_shutdown(&self) {
        // Join dispatch first: taking only the device lock could race a handler
        // that has not acquired it yet. The write lock also prevents late
        // handlers from entering while shutdown waits for cleanup.
        let joined = tokio::time::timeout(SHUTDOWN_JOIN_TIMEOUT, async {
            let mut accepting_calls = self.accepting_calls.write().await;
            *accepting_calls = false;
            // Input/video cancellation transfers this lock to bounded cleanup
            // tasks before dispatch returns. Acquiring it joins that cleanup too.
            self.gate.lock().await
        })
        .await;
        if joined.is_err() {
            tracing::warn!(
                "shutdown did not join in-flight device operations within {SHUTDOWN_JOIN_TIMEOUT:?}; exiting anyway"
            );
        }
        // Holding the gate when joined; otherwise best effort without it.
        let _device_idle = joined.ok();
        self.release_held_inputs().await;
    }

    /// Release the inputs this server still holds, and only those: another
    /// client's inputs stay pressed. Bounded by the cleanup timeout.
    async fn release_held_inputs(&self) {
        let held = std::mem::take(&mut *self.held.lock().unwrap());
        if held.is_empty() {
            return;
        }
        let release = async {
            for body in input::release_bodies(&held) {
                self.device
                    .json(Method::POST, "machine:input", &[], Some(body))
                    .await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        match tokio::time::timeout(RESUME_CLEANUP_TIMEOUT, release).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                tracing::warn!("releasing held inputs at shutdown failed: {error:#}")
            }
            Err(_) => tracing::warn!(
                "releasing held inputs at shutdown timed out; they may remain pressed"
            ),
        }
    }

    async fn request(
        &self,
        method: Method,
        route: &str,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> CallToolResult {
        let _guard = self.gate.lock().await;
        result(self.device.json(method, route, &query, body).await)
    }
    /// Like `request`, but also holds the cross-process device lock.
    async fn locked_request(&self, method: Method, route: &str) -> CallToolResult {
        result(
            async {
                let _guard = self.operation().await?;
                self.device.json(method, route, &[], None).await
            }
            .await,
        )
    }
    async fn drive_action(&self, drive: Drive, verb: &str) -> CallToolResult {
        self.request(
            Method::PUT,
            &format!("drives/{}:{verb}", drive.name()),
            vec![],
            None,
        )
        .await
    }
    async fn runner(&self, command: &str, p: Runner) -> CallToolResult {
        result(
            async {
                ensure!(
                    command == "sidplay" || (p.songnr.is_none() && p.song_lengths.is_none()),
                    "song options only apply to SID playback"
                );
                let mut query = vec![];
                if let Some(n) = p.songnr {
                    query.push(("songnr".into(), n.to_string()));
                }
                // Read local files before taking the device lock so slow file
                // I/O never blocks other device operations.
                let source = p.source.resolve().await?;
                if (command == "load_prg" || command == "run_prg")
                    && let ResolvedSource::Upload(_, ref bytes) = source
                {
                    validate_prg(bytes)?;
                }
                let song_lengths = match p.song_lengths {
                    None => None,
                    Some(extra) => match (&source, extra.resolve().await?) {
                        (ResolvedSource::Remote(_), _) => {
                            bail!("song-length uploads require an uploaded SID")
                        }
                        (_, ResolvedSource::Upload(name, bytes)) => Some((name, bytes)),
                        _ => bail!("song-length attachment must be local or base64"),
                    },
                };
                let route = format!("runners:{command}");
                let _guard = self.operation().await?;
                match source {
                    ResolvedSource::Remote(path) => {
                        query.push(("file".into(), path));
                        self.device.json(Method::PUT, &route, &query, None).await
                    }
                    ResolvedSource::Upload(name, bytes) => {
                        let mut files = vec![(name, bytes)];
                        files.extend(song_lengths);
                        self.device.upload(&route, &query, files).await
                    }
                }
            }
            .await,
        )
    }
}
fn result(value: Result<Value>) -> CallToolResult {
    match value {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => CallToolResult::error(vec![Content::text(format!("{error:#}"))]),
    }
}
const HEX_DIGITS: &[u8; 16] = b"0123456789ABCDEF";
fn segment(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for &b in value.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'~' | b'*' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(HEX_DIGITS[usize::from(b >> 4)] as char);
                out.push(HEX_DIGITS[usize::from(b & 0x0f)] as char);
            }
        }
    }
    out
}
fn device_path(path: &str) -> Result<String> {
    ensure!(
        path.starts_with('/') && path.len() > 1,
        "device path must be absolute, for example /Usb0/demo.prg"
    );
    ensure!(
        !path.chars().any(char::is_control),
        "device path contains control characters"
    );
    ensure!(
        !path.split('/').any(|part| part == "." || part == ".."),
        "relative path components are not allowed"
    );
    ensure!(
        !path[1..].split('/').any(str::is_empty),
        "device path may not contain empty components or a trailing slash"
    );
    Ok(path
        .trim_start_matches('/')
        .split('/')
        .map(segment)
        .collect::<Vec<_>>()
        .join("/"))
}
fn memory_range(address: &str, length: usize) -> Result<u16> {
    let text = address
        .strip_prefix('$')
        .or_else(|| address.strip_prefix("0x"))
        .or_else(|| address.strip_prefix("0X"))
        .unwrap_or(address);
    ensure!(
        !text.is_empty() && text.len() <= 4 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "address must be a 16-bit hexadecimal string"
    );
    let address = u16::from_str_radix(text, 16)?;
    ensure!(
        length > 0 && length <= 65536 - address as usize,
        "memory range must be nonempty and may not pass $FFFF"
    );
    Ok(address)
}
/// Parse a 24-bit hexadecimal REU address whose range must fit in 16 MiB.
fn reu_range(address: &str, length: usize) -> Result<u32> {
    let text = address
        .strip_prefix('$')
        .or_else(|| address.strip_prefix("0x"))
        .or_else(|| address.strip_prefix("0X"))
        .unwrap_or(address);
    ensure!(
        !text.is_empty() && text.len() <= 6 && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "REU address must be a 24-bit hexadecimal string"
    );
    let address = u32::from_str_radix(text, 16)?;
    ensure!(
        length > 0 && length <= REU_SIZE - address as usize,
        "REU range must be nonempty and may not pass $FFFFFF (16 MiB)"
    );
    Ok(address)
}
static REU_ZEROS: [u8; REU_CHUNK] = [0; REU_CHUNK];
fn reu_chunks<'a>(writes: &mut Vec<(u32, &'a [u8])>, address: u32, bytes: &'a [u8]) {
    for (index, chunk) in bytes.chunks(REU_CHUNK).enumerate() {
        writes.push((address + (index * REU_CHUNK) as u32, chunk));
    }
}
/// Order REU writes so a guard range (inside the written range) is zeroed
/// before anything else and receives its data only after everything else.
fn reu_writes(address: u32, data: &[u8], guard: Option<(u32, usize)>) -> Vec<(u32, &[u8])> {
    let mut writes = Vec::new();
    match guard {
        None => reu_chunks(&mut writes, address, data),
        Some((guard_address, guard_length)) => {
            for offset in (0..guard_length).step_by(REU_CHUNK) {
                let size = REU_CHUNK.min(guard_length - offset);
                writes.push((guard_address + offset as u32, &REU_ZEROS[..size]));
            }
            let start = (guard_address - address) as usize;
            let end = start + guard_length;
            reu_chunks(&mut writes, address, &data[..start]);
            reu_chunks(&mut writes, address + end as u32, &data[end..]);
            reu_chunks(&mut writes, guard_address, &data[start..end]);
        }
    }
    writes
}
/// Resumes the CPU if an REU upload is dropped (cancelled) while it is paused.
struct ResumeGuard {
    device: Device,
    armed: bool,
    operation_guard: Arc<OperationGuard>,
}
impl ResumeGuard {
    async fn resume(&mut self) -> Result<()> {
        let outcome = self
            .device
            .json(Method::PUT, "machine:resume", &[], None)
            .await;
        self.armed = false;
        outcome.map(|_| ())
    }
}
impl Drop for ResumeGuard {
    fn drop(&mut self) {
        if self.armed
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            let device = self.device.clone();
            let operation_guard = self.operation_guard.clone();
            runtime.spawn(async move {
                let _operation_guard = operation_guard;
                let resume = device.json(Method::PUT, "machine:resume", &[], None);
                match tokio::time::timeout(RESUME_CLEANUP_TIMEOUT, resume).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => tracing::warn!(
                        "resuming the C64 after a cancelled REU upload failed: {error:#}"
                    ),
                    Err(_) => tracing::warn!(
                        "resuming the C64 after a cancelled REU upload timed out; it may still be paused"
                    ),
                }
            });
        }
    }
}
fn unhex(text: &str) -> Result<Vec<u8>> {
    ensure!(
        text.len().is_multiple_of(2) && text.bytes().all(|b| b.is_ascii_hexdigit()),
        "data must contain pairs of hexadecimal digits, without spaces"
    );
    text.as_bytes()
        .chunks_exact(2)
        .map(|s| Ok(u8::from_str_radix(std::str::from_utf8(s)?, 16)?))
        .collect()
}
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_DIGITS[usize::from(b >> 4)] as char);
        out.push(HEX_DIGITS[usize::from(b & 0x0f)] as char);
    }
    out
}
fn validate_prg(bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() >= 3,
        "PRG must contain a two-byte load address and program data"
    );
    let address = u16::from_le_bytes([bytes[0], bytes[1]]);
    ensure!(
        bytes.len() - 2 <= 65536 - address as usize,
        "PRG payload exceeds the address space"
    );
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FileSource {
    Device { path: String },
    Local { path: String },
    Base64 { filename: String, data: String },
}
enum ResolvedSource {
    Remote(String),
    Upload(String, Vec<u8>),
}
impl FileSource {
    async fn resolve(self) -> Result<ResolvedSource> {
        let (name, bytes) = match self {
            Self::Device { path } => {
                device_path(&path)?;
                return Ok(ResolvedSource::Remote(path));
            }
            Self::Local { path } => {
                let path = std::path::Path::new(&path);
                // The server's working directory is unrelated to the client's,
                // so a relative path would silently pick an unexpected file.
                ensure!(
                    path.is_absolute(),
                    "local upload path must be absolute, for example C:/build/demo.prg or /home/me/demo.prg"
                );
                // Check the file type before opening: opening a FIFO or device
                // node can block indefinitely.
                let meta = tokio::fs::metadata(path)
                    .await
                    .context("cannot read local upload file")?;
                ensure!(
                    meta.is_file() && meta.len() <= MAX_FILE as u64,
                    "upload must be a regular file no larger than 32 MiB"
                );
                let file = tokio::fs::File::open(path)
                    .await
                    .context("cannot open local upload file")?;
                ensure!(
                    file.metadata().await?.is_file(),
                    "upload must be a regular file"
                );
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .context("file requires a UTF-8 filename")?
                    .to_owned();
                let mut bytes = Vec::new();
                file.take((MAX_FILE + 1) as u64)
                    .read_to_end(&mut bytes)
                    .await?;
                (name, bytes)
            }
            Self::Base64 { filename, data } => {
                ensure!(
                    data.len() <= MAX_FILE.div_ceil(3) * 4,
                    "base64 upload exceeds 32 MiB"
                );
                (
                    filename,
                    STANDARD.decode(data).context("invalid base64 upload")?,
                )
            }
        };
        ensure!(
            !name.is_empty() && !name.contains(['/', '\\']) && !name.chars().any(char::is_control),
            "upload filename must be a simple filename"
        );
        ensure!(
            !bytes.is_empty() && bytes.len() <= MAX_FILE,
            "upload size must be 1 byte through 32 MiB"
        );
        Ok(ResolvedSource::Upload(name, bytes))
    }
}

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Runner {
    pub source: FileSource,
    /// SID subsong number; omitted or zero selects the default.
    pub songnr: Option<u16>,
    /// Optional second upload for SID song lengths.
    pub song_lengths: Option<FileSource>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReuRange {
    /// Hexadecimal REU address, e.g. "0", "$560000", or "0x560000".
    pub address: String,
    pub length: usize,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UploadReu {
    /// Local or base64 file; device paths are not supported.
    pub source: FileSource,
    /// Hexadecimal 24-bit REU address, e.g. "0", "$560000", or "0x560000". The written range may not pass $FFFFFF; it is not checked against the REU size configured on the device.
    pub address: String,
    /// Byte offset of the data within the file; default 0.
    #[serde(default)]
    pub offset: usize,
    /// Bytes to write; default is the rest of the file after offset.
    pub length: Option<usize>,
    /// Reset the C64 before writing, so no running program can DMA into the REU during the upload; default true. After a reset only the KERNAL and BASIC run, and neither touches the REU. Pass false to feed data into a running program.
    #[serde(default = "default_true")]
    pub reset: bool,
    /// Pause the C64 CPU before writing (after any reset) and resume it afterwards, even if the write fails; default is the opposite of reset, so false after a reset and true without one. The CPU is resumed even if it was already paused before the call.
    pub pause: Option<bool>,
    /// Range inside the written range that is zeroed before any other write and receives its data last, so a program polling a header there never sees a stale or partial image. Live-feeding a running program this way needs reset: false.
    pub guard: Option<ReuRange>,
    /// PRG to reset, load and run after the upload and resume, exactly like run_prg.
    pub run_prg: Option<FileSource>,
}
fn default_true() -> bool {
    true
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigGet {
    pub category: Option<String>,
    pub item: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigSet {
    pub category: String,
    pub item: String,
    pub value: Value,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigBatch {
    pub settings: Value,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadMemory {
    /// Hexadecimal address, e.g. "0400", "$0400", or "0x0400".
    pub address: String,
    #[serde(default = "default_length")]
    pub length: usize,
}
fn default_length() -> usize {
    256
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteMemory {
    pub address: String,
    pub data_hex: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DebugValue {
    pub value: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Drive {
    A,
    B,
}
impl Drive {
    fn name(&self) -> &'static str {
        match self {
            Self::A => "a",
            Self::B => "b",
        }
    }
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DriveArg {
    pub drive: Drive,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ImageType {
    D64,
    G64,
    D71,
    G71,
    D81,
}
impl ImageType {
    fn name(&self) -> &'static str {
        match self {
            Self::D64 => "d64",
            Self::G64 => "g64",
            Self::D71 => "d71",
            Self::G71 => "g71",
            Self::D81 => "d81",
        }
    }
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MountMode {
    Readwrite,
    Readonly,
    Unlinked,
}
impl MountMode {
    fn name(&self) -> &'static str {
        match self {
            Self::Readwrite => "readwrite",
            Self::Readonly => "readonly",
            Self::Unlinked => "unlinked",
        }
    }
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub drive: Drive,
    pub source: FileSource,
    pub image_type: Option<ImageType>,
    pub mode: Option<MountMode>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Rom {
    pub drive: Drive,
    pub source: FileSource,
}
#[derive(Deserialize, JsonSchema)]
pub enum DriveMode {
    #[serde(rename = "1541")]
    D1541,
    #[serde(rename = "1571")]
    D1571,
    #[serde(rename = "1581")]
    D1581,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SetDriveMode {
    pub drive: Drive,
    pub mode: DriveMode,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    Video,
    Audio,
    Debug,
}
impl Stream {
    fn name(&self) -> &'static str {
        match self {
            Self::Video => "video",
            Self::Audio => "audio",
            Self::Debug => "debug",
        }
    }
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamStart {
    pub stream: Stream,
    pub destination: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StreamStop {
    pub stream: Stream,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FileInfo {
    pub path: String,
}
#[derive(Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DiskFormat {
    D64,
    D71,
    D81,
    Dnp,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateDisk {
    pub path: String,
    pub format: DiskFormat,
    pub tracks: Option<u16>,
    pub diskname: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InputEvents {
    pub events: Vec<input::InputEvent>,
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TypeText {
    pub text: String,
    #[serde(default = "default_key_delay")]
    pub hold_ms: u64,
    #[serde(default = "default_key_delay")]
    pub gap_ms: u64,
}
fn default_key_delay() -> u64 {
    50
}
#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextScreen {
    #[serde(default = "default_screen_base")]
    pub address: String,
    #[serde(default)]
    pub include_colors: bool,
}
fn default_screen_base() -> String {
    "0400".into()
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for C64Server {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        // Hold through dispatch destruction so shutdown cannot overtake a
        // handler that is about to acquire or transfer the device lock.
        let accepting_calls = self.accepting_calls.read().await;
        if !*accepting_calls {
            return Ok(CallToolResult::error(vec![Content::text("Server has shut down.")]).into());
        }
        // rmcp signals cancellation through this token; it does not abort the
        // handler. Dropping dispatch stops queued calls before they can acquire
        // the device lock and runs the active input/video cleanup guards.
        let cancelled = context.ct.clone();
        let call = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        tokio::select! {
            biased;
            _ = cancelled.cancelled() => Ok(CallToolResult::error(vec![Content::text(
                "Tool call cancelled; an action already sent to the device may have completed."
            )]).into()),
            result = self.tool_router.call(call) => result,
        }
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(rmcp::model::Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
            "C64 Ultimate control over LAN. Device paths and local upload files are distinct. Start with get_version/get_info. Firmware-dependent calls can be unavailable. Mutating calls can reset programs, overwrite memory/files, or change configuration. Screenshots temporarily control the video stream and may interrupt debug streaming. No hardware action occurs during startup.")
    }
}

#[tool_router]
impl C64Server {
    #[tool(
        description = "Read the REST API version.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_version(&self) -> CallToolResult {
        self.request(Method::GET, "version", vec![], None).await
    }
    #[tool(
        description = "Read device product, firmware and identity.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_info(&self) -> CallToolResult {
        self.request(Method::GET, "info", vec![], None).await
    }
    #[tool(
        description = "List configuration categories.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn list_config_categories(&self) -> CallToolResult {
        self.request(Method::GET, "configs", vec![], None).await
    }
    #[tool(
        description = "Replace current settings with saved flash settings.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn load_config_from_flash(&self) -> CallToolResult {
        self.request(Method::PUT, "configs:load_from_flash", vec![], None)
            .await
    }
    #[tool(
        description = "Persist all current configuration to nonvolatile flash.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn save_config_to_flash(&self) -> CallToolResult {
        self.request(Method::PUT, "configs:save_to_flash", vec![], None)
            .await
    }
    #[tool(
        description = "Reset current configuration to defaults; does not save to flash.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn reset_config_defaults(&self) -> CallToolResult {
        self.request(Method::PUT, "configs:reset_to_default", vec![], None)
            .await
    }
    #[tool(
        description = "Reset the C64; current program state is lost.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn reset_machine(&self) -> CallToolResult {
        self.locked_request(Method::PUT, "machine:reset").await
    }
    #[tool(
        description = "Restart the machine and reinitialize cartridge configuration.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn reboot_machine(&self) -> CallToolResult {
        self.locked_request(Method::PUT, "machine:reboot").await
    }
    #[tool(
        description = "Pause the CPU until resume_machine; timers may continue.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn pause_machine(&self) -> CallToolResult {
        self.locked_request(Method::PUT, "machine:pause").await
    }
    #[tool(
        description = "Resume the paused CPU.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn resume_machine(&self) -> CallToolResult {
        self.locked_request(Method::PUT, "machine:resume").await
    }
    #[tool(
        description = "Power off supported hardware. A disconnect leaves the outcome uncertain.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn poweroff_machine(&self) -> CallToolResult {
        self.locked_request(Method::PUT, "machine:poweroff").await
    }
    #[tool(
        description = "Toggle the firmware menu. While open, injected keys reach the menu.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn toggle_menu(&self) -> CallToolResult {
        self.request(Method::PUT, "machine:menu_button", vec![], None)
            .await
    }
    #[tool(
        description = "Read the U64 debug register; firmware-dependent.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn read_debug_register(&self) -> CallToolResult {
        self.request(Method::GET, "machine:debugreg", vec![], None)
            .await
    }
    #[tool(
        description = "Read drive status and mounted image paths.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn list_drives(&self) -> CallToolResult {
        self.request(Method::GET, "drives", vec![], None).await
    }
    #[tool(
        description = "Read REST-injected keyboard and joystick state; firmware-dependent.",
        annotations(read_only_hint = true, destructive_hint = false)
    )]
    async fn get_input_state(&self) -> CallToolResult {
        self.request(Method::GET, "machine:input", vec![], None)
            .await
    }
    #[tool(
        description = "Reset and load a PRG without running it.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn load_prg(&self, Parameters(p): Parameters<Runner>) -> CallToolResult {
        self.runner("load_prg", p).await
    }
    #[tool(
        description = "Reset, load and run a PRG.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn run_prg(&self, Parameters(p): Parameters<Runner>) -> CallToolResult {
        self.runner("run_prg", p).await
    }
    #[tool(
        description = "Reset and activate a cartridge image temporarily.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn run_crt(&self, Parameters(p): Parameters<Runner>) -> CallToolResult {
        self.runner("run_crt", p).await
    }
    #[tool(
        description = "Play SID music; can replace current program state.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn play_sid(&self, Parameters(p): Parameters<Runner>) -> CallToolResult {
        self.runner("sidplay", p).await
    }
    #[tool(
        description = "Play an Amiga MOD; requires sampler support and overwrites REU content.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn play_mod(&self, Parameters(p): Parameters<Runner>) -> CallToolResult {
        self.runner("modplay", p).await
    }
    #[tool(
        description = "Reset a floppy drive.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn reset_drive(&self, Parameters(p): Parameters<DriveArg>) -> CallToolResult {
        self.drive_action(p.drive, "reset").await
    }
    #[tool(
        description = "Eject the mounted disk.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn eject_disk(&self, Parameters(p): Parameters<DriveArg>) -> CallToolResult {
        self.drive_action(p.drive, "remove").await
    }
    #[tool(
        description = "Detach the backing image; subsequent disk writes will not persist to it.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn unlink_disk(&self, Parameters(p): Parameters<DriveArg>) -> CallToolResult {
        self.drive_action(p.drive, "unlink").await
    }
    #[tool(
        description = "Enable the drive; resets it if already enabled.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn enable_drive(&self, Parameters(p): Parameters<DriveArg>) -> CallToolResult {
        self.drive_action(p.drive, "on").await
    }
    #[tool(
        description = "Disable a floppy drive.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn disable_drive(&self, Parameters(p): Parameters<DriveArg>) -> CallToolResult {
        self.drive_action(p.drive, "off").await
    }

    #[tool(
        description = "Read configuration values or item metadata. Category/item patterns support firmware wildcards.",
        annotations(read_only_hint = true)
    )]
    async fn get_config(&self, Parameters(p): Parameters<ConfigGet>) -> CallToolResult {
        let route = match (p.category, p.item) {
            (None, None) => "configs".into(),
            (Some(c), None) if !c.is_empty() => format!("configs/{}", segment(&c)),
            (Some(c), Some(i)) if !c.is_empty() && !i.is_empty() => {
                format!("configs/{}/{}", segment(&c), segment(&i))
            }
            _ => {
                return result(Err(anyhow::anyhow!(
                    "item requires a nonempty category and item"
                )));
            }
        };
        self.request(Method::GET, &route, vec![], None).await
    }
    #[tool(
        description = "Set one configuration value (string or integer); wildcard paths can affect multiple items. Does not save flash.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn set_config(&self, Parameters(p): Parameters<ConfigSet>) -> CallToolResult {
        let value = match config_value(&p.value) {
            Ok(v) => v,
            Err(e) => return result(Err(e)),
        };
        if p.category.is_empty() || p.item.is_empty() {
            return result(Err(anyhow::anyhow!("category and item must be nonempty")));
        }
        self.request(
            Method::PUT,
            &format!("configs/{}/{}", segment(&p.category), segment(&p.item)),
            vec![("value".into(), value)],
            None,
        )
        .await
    }
    #[tool(
        description = "Apply category/item/value configuration updates. Failure can leave partially applied settings; no automatic rollback or flash save.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn set_config_batch(&self, Parameters(p): Parameters<ConfigBatch>) -> CallToolResult {
        if let Err(e) = validate_config_batch(&p.settings) {
            return result(Err(e));
        }
        self.request(Method::POST, "configs", vec![], Some(p.settings))
            .await
    }
    #[tool(
        description = "Read bank-dependent C64 memory, returning hex and base64. Address is hexadecimal; length defaults to 256.",
        annotations(read_only_hint = true)
    )]
    async fn read_memory(&self, Parameters(p): Parameters<ReadMemory>) -> CallToolResult {
        let _guard = self.gate.lock().await;
        result(async {
            let address=memory_range(&p.address,p.length)?;
            let bytes=self.device.binary("machine:readmem",&[("address".into(),format!("{address:04X}")),("length".into(),p.length.to_string())]).await?;
            ensure!(bytes.len()==p.length,"short or oversized memory response: expected {}, received {}",p.length,bytes.len());
            Ok(json!({"address":format!("{address:04X}"),"length":bytes.len(),"data_hex":hex(&bytes),"data_base64":STANDARD.encode(&bytes)}))
        }.await)
    }
    #[tool(
        description = "Write bytes into bank-dependent C64 memory. data_hex contains byte pairs without spaces. Can overwrite running code or mapped I/O.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn write_memory(&self, Parameters(p): Parameters<WriteMemory>) -> CallToolResult {
        let _guard = self.gate.lock().await;
        result(
            async {
                ensure!(p.data_hex.len() <= 131072, "memory data exceeds 64 KiB");
                let bytes = unhex(&p.data_hex)?;
                let address = memory_range(&p.address, bytes.len())?;
                let mut query = vec![("address".into(), format!("{address:04X}"))];
                if bytes.len() <= 128 {
                    query.push(("data".into(), hex(&bytes)));
                    self.device
                        .json(Method::PUT, "machine:writemem", &query, None)
                        .await
                } else {
                    self.device
                        .write_bytes("machine:writemem", &query, bytes)
                        .await
                }
            }
            .await,
        )
    }
    #[tool(
        description = "Write a local or base64 file (optionally an offset/length slice, up to 16 MiB) into REU memory at a 24-bit hexadecimal address over the Ultimate DMA service (TCP port 64). Resets the C64 first by default, so no running program can DMA into the REU during the upload; pass reset=false to feed a running program. pause (default: true only when reset is false) pauses the CPU during the write and always resumes it afterwards, even if it was already paused. Writes beyond the configured REU size are not detected. An optional guard range inside the written range is zeroed first and written last. Optional run_prg then resets and runs a PRG.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn upload_reu(&self, Parameters(p): Parameters<UploadReu>) -> CallToolResult {
        result(
            async {
                // Validate everything and read local files before touching the
                // device, so no error can leave the CPU paused or the REU half written.
                let bytes = match p.source.resolve().await? {
                    ResolvedSource::Upload(_, bytes) => bytes,
                    ResolvedSource::Remote(_) => {
                        bail!("REU upload requires a local or base64 source")
                    }
                };
                ensure!(
                    p.offset < bytes.len(),
                    "offset must lie within the {}-byte file",
                    bytes.len()
                );
                let available = bytes.len() - p.offset;
                let length = p.length.unwrap_or(available);
                ensure!(
                    length > 0 && length <= available,
                    "length must be nonzero and fit in the file after offset ({available} bytes)"
                );
                let address = reu_range(&p.address, length)?;
                let data = &bytes[p.offset..p.offset + length];
                let guard = match p.guard {
                    None => None,
                    Some(guard) => {
                        let guard_address = reu_range(&guard.address, guard.length)?;
                        ensure!(
                            guard_address >= address
                                && guard_address as usize + guard.length
                                    <= address as usize + length,
                            "guard range must lie inside the written REU range"
                        );
                        Some((guard_address, guard.length))
                    }
                };
                let program = match p.run_prg {
                    None => None,
                    Some(source) => {
                        let source = source.resolve().await?;
                        if let ResolvedSource::Upload(_, ref bytes) = source {
                            validate_prg(bytes)?;
                        }
                        Some(source)
                    }
                };
                let writes = reu_writes(address, data, guard);
                let guard_chunks = guard.map_or(0, |(_, length)| length.div_ceil(REU_CHUNK));

                let reset = p.reset;
                let pause = p.pause.unwrap_or(!reset);

                let operation_guard = Arc::new(self.operation().await?);
                // Reset first, so the old program cannot DMA into the REU while
                // it is written. Nothing is armed yet, so failure needs no cleanup.
                if reset {
                    self.device
                        .json(Method::PUT, "machine:reset", &[], None)
                        .await
                        .context("could not reset the C64; nothing was written (a timed-out reset may still have happened)")?;
                }
                // Arm the resume before sending the pause: a pause that times
                // out, or a call cancelled while it is in flight, may still have
                // paused the CPU. Resuming a running CPU is harmless.
                let mut resume = pause.then(|| ResumeGuard {
                    device: self.device.clone(),
                    armed: true,
                    operation_guard: operation_guard.clone(),
                });
                if let Some(guard) = resume.as_mut()
                    && let Err(pause) = self
                        .device
                        .json(Method::PUT, "machine:pause", &[], None)
                        .await
                {
                    return Err(match guard.resume().await {
                        Ok(()) => pause.context("could not pause the C64; nothing was written"),
                        Err(resume) => anyhow::anyhow!(
                            "could not pause the C64; nothing was written: {pause:#}; resuming the C64 also failed, so it may be paused: {resume:#}"
                        ),
                    });
                }
                let written = self.device.reu_write(&writes).await;
                let resumed = match resume.as_mut() {
                    Some(guard) => Some(guard.resume().await),
                    None => None,
                };
                match (written, resumed) {
                    (Err(write), Some(Err(resume))) => bail!(
                        "{write:#}; resuming the C64 also failed, so it may still be paused: {resume:#}"
                    ),
                    (Err(write), Some(Ok(()))) => {
                        return Err(write.context("REU upload failed; the C64 was resumed"));
                    }
                    (Err(write), None) => return Err(write.context("REU upload failed")),
                    (Ok(()), Some(Err(resume))) => {
                        return Err(resume.context(
                            "REU upload completed, but resuming the C64 failed; it may still be paused",
                        ));
                    }
                    (Ok(()), _) => {}
                }
                let ran = match program {
                    None => false,
                    Some(program) => {
                        match program {
                            ResolvedSource::Remote(path) => {
                                self.device
                                    .json(
                                        Method::PUT,
                                        "runners:run_prg",
                                        &[("file".into(), path)],
                                        None,
                                    )
                                    .await
                            }
                            ResolvedSource::Upload(name, bytes) => {
                                self.device
                                    .upload("runners:run_prg", &[], vec![(name, bytes)])
                                    .await
                            }
                        }
                        .context("REU upload completed, but run_prg failed")?;
                        true
                    }
                };
                Ok(json!({
                    "address": format!("{address:06X}"),
                    "bytes_written": length,
                    "chunks": writes.len() - guard_chunks,
                    "guard": guard.map(|(guard_address, guard_length)| json!({
                        "address": format!("{guard_address:06X}"),
                        "length": guard_length,
                        "zero_chunks": guard_chunks,
                    })),
                    "reset": reset,
                    "paused": pause,
                    "ran": ran,
                }))
            }
            .await,
        )
    }
    #[tool(
        description = "Write a hexadecimal byte to the U64 debug register and return its readback.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn write_debug_register(&self, Parameters(p): Parameters<DebugValue>) -> CallToolResult {
        let value = match memory_range(&p.value, 1) {
            Ok(v) if v <= 255 => v,
            _ => {
                return result(Err(anyhow::anyhow!(
                    "debug register requires a hexadecimal byte"
                )));
            }
        };
        self.request(
            Method::PUT,
            "machine:debugreg",
            vec![("value".into(), format!("{value:02X}"))],
            None,
        )
        .await
    }
    #[tool(
        description = "Mount a device or uploaded disk image. Default mode is readonly; use readwrite or unlinked explicitly when needed.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn mount_disk(&self, Parameters(p): Parameters<Mount>) -> CallToolResult {
        result(
            async {
                let route = format!("drives/{}:mount", p.drive.name());
                let mut query = vec![(
                    "mode".into(),
                    p.mode.unwrap_or(MountMode::Readonly).name().into(),
                )];
                if let Some(t) = p.image_type {
                    query.push(("type".into(), t.name().into()));
                }
                let source = p.source.resolve().await?;
                let _guard = self.gate.lock().await;
                match source {
                    ResolvedSource::Remote(path) => {
                        query.push(("image".into(), path));
                        self.device.json(Method::PUT, &route, &query, None).await
                    }
                    ResolvedSource::Upload(name, bytes) => {
                        self.device
                            .upload(&route, &query, vec![(name, bytes)])
                            .await
                    }
                }
            }
            .await,
        )
    }
    #[tool(
        description = "Temporarily load a 16 or 32 KiB drive ROM. Drive mode changes or reboot replace it.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn load_drive_rom(&self, Parameters(p): Parameters<Rom>) -> CallToolResult {
        result(
            async {
                let route = format!("drives/{}:load_rom", p.drive.name());
                let source = p.source.resolve().await?;
                if let ResolvedSource::Upload(_, ref bytes) = source {
                    ensure!(
                        bytes.len() == 16384 || bytes.len() == 32768,
                        "drive ROM must be 16 or 32 KiB"
                    );
                }
                let _guard = self.gate.lock().await;
                match source {
                    ResolvedSource::Remote(path) => {
                        self.device
                            .json(Method::PUT, &route, &[("file".into(), path)], None)
                            .await
                    }
                    ResolvedSource::Upload(name, bytes) => {
                        self.device.upload(&route, &[], vec![(name, bytes)]).await
                    }
                }
            }
            .await,
        )
    }
    #[tool(
        description = "Change drive emulation mode and reload its default ROM.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn set_drive_mode(&self, Parameters(p): Parameters<SetDriveMode>) -> CallToolResult {
        let mode = match p.mode {
            DriveMode::D1541 => "1541",
            DriveMode::D1571 => "1571",
            DriveMode::D1581 => "1581",
        };
        self.request(
            Method::PUT,
            &format!("drives/{}:set_mode", p.drive.name()),
            vec![("mode".into(), mode.into())],
            None,
        )
        .await
    }
    #[tool(
        description = "Start a UDP stream to an IPv4 address with optional port. Requires supported hardware and Ethernet; video interrupts debug streaming.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn start_stream(&self, Parameters(p): Parameters<StreamStart>) -> CallToolResult {
        if p.destination.parse::<Ipv4Addr>().is_err()
            && !p
                .destination
                .parse::<std::net::SocketAddrV4>()
                .is_ok_and(|a| a.port() != 0)
        {
            return result(Err(anyhow::anyhow!(
                "destination must be IPv4 or IPv4:port"
            )));
        }
        result(
            async {
                let _guard = self.operation().await?;
                let value = self
                    .device
                    .json(
                        Method::PUT,
                        &format!("streams/{}:start", p.stream.name()),
                        &[("ip".into(), p.destination.clone())],
                        None,
                    )
                    .await?;
                if let Stream::Video = p.stream {
                    *self.video_stream.lock().unwrap() = Some(p.destination);
                }
                Ok(value)
            }
            .await,
        )
    }
    #[tool(
        description = "Stop a video, audio or debug UDP stream.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn stop_stream(&self, Parameters(p): Parameters<StreamStop>) -> CallToolResult {
        result(
            async {
                let _guard = self.operation().await?;
                if let Stream::Video = p.stream {
                    // Even a failed stop means the caller no longer wants it.
                    *self.video_stream.lock().unwrap() = None;
                }
                self.device
                    .json(
                        Method::PUT,
                        &format!("streams/{}:stop", p.stream.name()),
                        &[],
                        None,
                    )
                    .await
            }
            .await,
        )
    }
    #[tool(
        description = "Read metadata for an absolute device file path. Firmware-dependent, not a directory listing.",
        annotations(read_only_hint = true)
    )]
    async fn file_info(&self, Parameters(p): Parameters<FileInfo>) -> CallToolResult {
        let path = match device_path(&p.path) {
            Ok(v) => v,
            Err(e) => return result(Err(e)),
        };
        self.request(Method::GET, &format!("files/{path}:info"), vec![], None)
            .await
    }
    #[tool(
        description = "Create a disk image at an absolute device path. May overwrite an existing file. D64 defaults to 35 tracks (35 or 40 supported); DNP requires 1..255.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn create_disk_image(&self, Parameters(p): Parameters<CreateDisk>) -> CallToolResult {
        let validated = (|| -> Result<_> {
            let path = device_path(&p.path)?;
            let mut query = vec![];
            let format = match p.format {
                DiskFormat::D64 => {
                    let n = p.tracks.unwrap_or(35);
                    ensure!(n == 35 || n == 40, "D64 tracks must be 35 or 40");
                    query.push(("tracks".into(), n.to_string()));
                    "d64"
                }
                DiskFormat::D71 => {
                    ensure!(p.tracks.is_none(), "D71 has a fixed track count");
                    "d71"
                }
                DiskFormat::D81 => {
                    ensure!(p.tracks.is_none(), "D81 has a fixed track count");
                    "d81"
                }
                DiskFormat::Dnp => {
                    let n = p.tracks.context("DNP requires tracks")?;
                    ensure!((1..=255).contains(&n), "DNP tracks must be 1..255");
                    query.push(("tracks".into(), n.to_string()));
                    "dnp"
                }
            };
            if let Some(name) = p.diskname {
                query.push(("diskname".into(), name));
            }
            Ok((format!("files/{path}:create_{format}"), query))
        })();
        match validated {
            Ok((route, q)) => self.request(Method::PUT, &route, q, None).await,
            Err(e) => result(Err(e)),
        }
    }
    #[tool(
        description = "Inject a validated batch of keyboard/joystick events. Held inputs persist until released. Firmware-dependent.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn send_input(&self, Parameters(p): Parameters<InputEvents>) -> CallToolResult {
        let body = match input::validate_events(&p.events) {
            Ok(body) => body,
            Err(e) => return result(Err(e)),
        };
        let _guard = self.gate.lock().await;
        // Record presses before sending: a lost or cancelled response may
        // still have pressed them, and shutdown must release them.
        self.held.lock().unwrap().extend(input::pressed(&p.events));
        let sent = self
            .device
            .json(Method::POST, "machine:input", &[], Some(body))
            .await;
        if sent.is_ok() {
            input::apply(&mut self.held.lock().unwrap(), &p.events);
        }
        result(sent)
    }
    #[tool(
        description = "Release all REST-injected keyboard and joystick inputs.",
        annotations(read_only_hint = false, destructive_hint = false)
    )]
    async fn release_all_inputs(&self) -> CallToolResult {
        let _guard = self.gate.lock().await;
        let sent = self
            .device
            .json(
                Method::POST,
                "machine:input",
                &[],
                Some(json!({"events":[{"kind":"release_all"}]})),
            )
            .await;
        if sent.is_ok() {
            self.held.lock().unwrap().clear();
        }
        result(sent)
    }
    #[tool(
        description = "Type supported ASCII using paced physical C64 keys, default 50 ms held/50 ms gap. Letter case folds to unshifted keys; use raw events for exact modifiers. Firmware-dependent; no RAM fallback.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn type_text(&self, Parameters(p): Parameters<TypeText>) -> CallToolResult {
        let guard = match self.operation().await {
            Ok(guard) => Arc::new(guard),
            Err(error) => return result(Err(error)),
        };
        // Keys the caller holds (a modifier, say) must stay held after typing.
        let held: HashSet<String> = self
            .held
            .lock()
            .unwrap()
            .iter()
            .filter(|(port, _)| *port == 0)
            .map(|(_, name)| name.clone())
            .collect();
        result(
            input::type_text(
                &self.device,
                &p.text,
                p.hold_ms,
                p.gap_ms,
                &held,
                Some(guard),
            )
            .await,
        )
    }
    #[tool(
        description = "Read and decode the 40x25 firmware menu screen. A 404 can mean the menu is closed; firmware-dependent.",
        annotations(read_only_hint = true)
    )]
    async fn read_menu_screen(&self) -> CallToolResult {
        let _guard = self.gate.lock().await;
        result(
            async { screen::decode_menu(&self.device.binary("machine:menu_screen", &[]).await?) }
                .await,
        )
    }
    #[tool(
        description = "Read a 40x25 text-memory snapshot (default $0400), optionally color RAM. Not a screenshot: custom glyphs, bitmap graphics, sprites and raster effects are not represented.",
        annotations(read_only_hint = true)
    )]
    async fn read_text_screen(&self, Parameters(p): Parameters<TextScreen>) -> CallToolResult {
        let _guard = self.gate.lock().await;
        result(
            async {
                let address = memory_range(&p.address, 1000)?;
                let bytes = self
                    .device
                    .binary(
                        "machine:readmem",
                        &[
                            ("address".into(), format!("{address:04X}")),
                            ("length".into(), "1000".into()),
                        ],
                    )
                    .await?;
                let colors = if p.include_colors {
                    Some(
                        self.device
                            .binary(
                                "machine:readmem",
                                &[
                                    ("address".into(), "D800".into()),
                                    ("length".into(), "1000".into()),
                                ],
                            )
                            .await?,
                    )
                } else {
                    None
                };
                let mut value = screen::decode_text(&bytes, colors.as_deref())?;
                value["address"] = json!(format!("{address:04X}"));
                Ok(value)
            }
            .await,
        )
    }
    #[tool(
        description = "Capture a complete PAL/NTSC video frame as PNG. Takes over the video destination and can interrupt debug streaming. Afterwards it restarts a video stream this server started with start_stream (reported as restored_stream), and otherwise stops video. Requires Ethernet and a reachable capture UDP port.",
        annotations(read_only_hint = false, destructive_hint = true)
    )]
    async fn capture_screenshot(&self) -> CallToolResult {
        let guard = match self.operation().await {
            Ok(guard) => Arc::new(guard),
            Err(error) => return result(Err(error)),
        };
        let restore = self.video_stream.lock().unwrap().clone();
        match video::capture(
            &self.device,
            self.capture_ip,
            self.capture_port,
            self.capture_timeout,
            Some(guard),
            restore.clone(),
        )
        .await
        {
            Ok(png) => {
                let mut content = vec![Content::image(STANDARD.encode(png), "image/png")];
                if let Some(destination) = restore {
                    content.push(Content::text(
                        json!({ "restored_stream": destination }).to_string(),
                    ));
                }
                CallToolResult::success(content)
            }
            Err(e) => result(Err(e)),
        }
    }
}
fn config_value(value: &Value) -> Result<String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Number(n) if n.is_i64() || n.is_u64() => Ok(n.to_string()),
        _ => bail!("configuration values must be strings or integers"),
    }
}
fn validate_config_batch(settings: &Value) -> Result<()> {
    let categories = settings
        .as_object()
        .context("settings must be a category/item/value object")?;
    ensure!(!categories.is_empty(), "settings must not be empty");
    for (category, items) in categories {
        ensure!(!category.is_empty(), "category must not be empty");
        let items = items
            .as_object()
            .context("each category must contain an item/value object")?;
        ensure!(!items.is_empty(), "category items must not be empty");
        for (item, value) in items {
            ensure!(!item.is_empty(), "item must not be empty");
            config_value(value)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_path_encodes_segments_and_rejects_malformed_paths() {
        assert_eq!(
            device_path("/Usb0/my game.prg").unwrap(),
            "Usb0/my%20game%2Eprg"
        );
        for bad in [
            "Usb0/x",
            "/",
            "/Usb0/",
            "/Usb0//x.d64",
            "/Usb0/../x",
            "/Usb0/./x",
        ] {
            assert!(device_path(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn hex_round_trips() {
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(unhex(&hex(&bytes)).unwrap(), bytes);
        assert_eq!(hex(&[0x0a, 0xff]), "0AFF");
    }

    #[test]
    fn reu_range_accepts_24_bit_addresses_within_16_mib() {
        assert_eq!(reu_range("0", 1).unwrap(), 0);
        assert_eq!(reu_range("$560000", 16).unwrap(), 0x560000);
        assert_eq!(reu_range("0xFFFFFF", 1).unwrap(), 0xFFFFFF);
        assert_eq!(reu_range("0", REU_SIZE).unwrap(), 0);
        for (address, length) in [
            ("1000000", 1),
            ("FFFFFF", 2),
            ("1", REU_SIZE),
            ("0", 0),
            ("", 1),
            ("$", 1),
            ("12G", 1),
        ] {
            assert!(
                reu_range(address, length).is_err(),
                "{address}/{length} should be rejected"
            );
        }
    }

    #[test]
    fn reu_writes_zero_the_guard_first_and_write_it_last() {
        let data: Vec<u8> = (0..70000u32).map(|i| i as u8).collect();
        let plain = reu_writes(0x560000, &data, None);
        assert_eq!(
            plain.iter().map(|(a, b)| (*a, b.len())).collect::<Vec<_>>(),
            [(0x560000, 32768), (0x568000, 32768), (0x570000, 4464)]
        );
        let guarded = reu_writes(0x560000, &data, Some((0x560010, 40000)));
        assert_eq!(
            guarded
                .iter()
                .map(|(a, b)| (*a, b.len()))
                .collect::<Vec<_>>(),
            [
                (0x560010, 32768),
                (0x568010, 7232),
                (0x560000, 16),
                (0x569C50, 29984),
                (0x560010, 32768),
                (0x568010, 7232),
            ]
        );
        assert!(guarded[..2].iter().all(|(_, b)| b.iter().all(|&x| x == 0)));
        let mut image = vec![0xEEu8; data.len()];
        for (address, bytes) in &guarded[2..] {
            let start = (*address - 0x560000) as usize;
            image[start..start + bytes.len()].copy_from_slice(bytes);
        }
        assert_eq!(image, data);
    }

    #[tokio::test]
    async fn local_upload_requires_absolute_regular_file() {
        let relative = FileSource::Local {
            path: "game.prg".into(),
        };
        let error = relative.resolve().await.err().unwrap();
        assert!(format!("{error:#}").contains("must be absolute"));
        let directory = FileSource::Local {
            path: std::env::temp_dir().to_string_lossy().into_owned(),
        };
        let error = directory.resolve().await.err().unwrap();
        assert!(format!("{error:#}").contains("regular file"));
    }
}
