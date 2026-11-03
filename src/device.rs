//! Bounded, authenticated access to one configured Ultimate REST API.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{
    Client, Method, RequestBuilder, Response, StatusCode, Url,
    header::{CONTENT_TYPE, HeaderMap, HeaderValue},
    multipart::{Form, Part},
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// Applies to response bodies and uploaded request payloads.
pub const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;
/// The Ultimate's DMA service listens on this TCP port of the REST host.
pub const DMA_PORT: u16 = 64;
/// REU addresses are 24-bit: the largest RAM Expansion Unit is 16 MiB.
pub const REU_SIZE: usize = 16 * 1024 * 1024;
/// Largest data payload of one DMA REU write command.
pub const REU_CHUNK: usize = 32 * 1024;

/// Bound on the IDENTIFY probe sent before any write: a busy or dead DMA
/// service should fail fast, while nothing has been written yet.
const DMA_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

const DMA_REU_WRITE: u16 = 0xff07;
const DMA_IDENTIFY: u16 = 0xff0e;
const DMA_AUTHENTICATE: u16 = 0xff1f;

/// Clones share the HTTP connection pool; no connection is made at construction.
#[derive(Clone)]
pub struct Device {
    client: Client,
    base: Option<Url>,
    password: Option<Arc<str>>,
    timeout: Duration,
    dma_port: u16,
}

impl Device {
    pub fn new(host: Option<&str>, password: Option<&str>, timeout: Duration) -> Result<Self> {
        ensure!(
            !timeout.is_zero(),
            "Device timeout must be greater than zero"
        );
        let base = host.map(normalize_host).transpose()?;
        let mut headers = HeaderMap::new();
        let password = password.filter(|value| !value.is_empty());
        if let Some(password) = password {
            let mut value = HeaderValue::from_str(password)
                .map_err(|_| anyhow!("C64U_PASSWORD is not a valid HTTP header value"))?;
            value.set_sensitive(true);
            headers.insert("X-Password", value);
        }
        let client = Client::builder()
            .default_headers(headers)
            .timeout(timeout)
            .connect_timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .build()
            .map_err(|_| anyhow!("Could not initialize the device HTTP client"))?;
        Ok(Self {
            client,
            base,
            password: password.map(Arc::from),
            timeout,
            dma_port: DMA_PORT,
        })
    }

    /// Use another TCP port for the DMA service; intended for loopback tests.
    #[doc(hidden)]
    pub fn with_dma_port(mut self, port: u16) -> Self {
        self.dma_port = port;
        self
    }

    /// Normalized API root, ending in `/v1/`.
    pub fn base_url(&self) -> Option<&Url> {
        self.base.as_ref()
    }

    pub async fn json(
        &self,
        method: Method,
        route: &str,
        query: &[(String, String)],
        body: Option<Value>,
    ) -> Result<Value> {
        let mut request = self.request(method, route, query)?;
        if let Some(body) = body {
            let bytes = serde_json::to_vec(&body)
                .map_err(|_| anyhow!("Could not serialize the JSON request"))?;
            check_size(bytes.len(), "Request")?;
            request = request.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        self.json_response(request).await
    }

    pub async fn binary(&self, route: &str, query: &[(String, String)]) -> Result<Vec<u8>> {
        let response = self.send(self.request(Method::GET, route, query)?).await?;
        let status = response.status();
        let mime = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(|value| value.split(';').next().unwrap_or("").trim().to_owned());
        let is_json = mime.as_deref().is_some_and(|mime| {
            mime.eq_ignore_ascii_case("application/json") || mime.ends_with("+json")
        });
        let is_binary = mime
            .as_deref()
            .is_some_and(|mime| mime.eq_ignore_ascii_case("application/octet-stream"));
        let bytes = read_bounded(response).await?;
        if !status.is_success() || is_json {
            self.decode_json(status, &bytes)?;
            bail!("Device returned JSON when a binary response was expected");
        }
        // Some firmware errors omit Content-Type. Do not expose an error envelope
        // as memory bytes, but preserve legitimate binary data otherwise.
        if !is_binary
            && let Ok(value) = serde_json::from_slice::<Value>(&bytes)
            && value.is_object()
            && value.get("errors").is_some()
        {
            self.decode_json(status, &bytes)?;
            bail!("Device returned JSON when a binary response was expected");
        }
        Ok(bytes)
    }

    /// Files are `(filename, contents)` pairs in attachment order.
    pub async fn upload(
        &self,
        route: &str,
        query: &[(String, String)],
        files: Vec<(String, Vec<u8>)>,
    ) -> Result<Value> {
        ensure!(
            !files.is_empty(),
            "At least one upload attachment is required"
        );
        ensure!(
            files.len() <= 16,
            "Too many upload attachments (maximum 16)"
        );
        let mut size = 128usize;
        let mut form = Form::new();
        for (filename, bytes) in files {
            ensure!(
                !filename.is_empty() && filename.len() <= 1024,
                "Upload filenames must contain 1 to 1024 bytes"
            );
            ensure!(
                !filename.contains(['\r', '\n', '\0']),
                "Upload filename contains invalid characters"
            );
            // Conservative allowance for percent-encoded filenames and MIME
            // headers keeps the complete multipart request under the body cap.
            size = size
                .checked_add(bytes.len())
                .and_then(|size| size.checked_add(filename.len() * 3 + 512))
                .ok_or_else(|| anyhow!("Upload exceeds the 32 MiB body limit"))?;
            check_size(size, "Upload")?;
            form = form.part(
                "file",
                Part::bytes(bytes)
                    .file_name(filename)
                    .mime_str("application/octet-stream")?,
            );
        }
        let request = self.request(Method::POST, route, query)?.multipart(form);
        self.json_response(request).await
    }

    pub async fn write_bytes(
        &self,
        route: &str,
        query: &[(String, String)],
        bytes: Vec<u8>,
    ) -> Result<Value> {
        check_size(bytes.len(), "Upload")?;
        let request = self
            .request(Method::POST, route, query)?
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(bytes);
        self.json_response(request).await
    }

    /// Write `(REU address, bytes)` blocks to REU memory over the DMA service
    /// socket, in order, each at most [`REU_CHUNK`] bytes. A configured
    /// password authenticates the socket, and an IDENTIFY probe with a short
    /// bound checks that the service answers before anything is written.
    /// Returns after the device answers an IDENTIFY sent behind the last
    /// write, so every write has been processed.
    pub async fn reu_write(&self, writes: &[(u32, &[u8])]) -> Result<()> {
        let base = self.base.as_ref().ok_or_else(no_host)?;
        for (address, bytes) in writes {
            ensure!(
                !bytes.is_empty()
                    && bytes.len() <= REU_CHUNK
                    && *address as usize + bytes.len() <= REU_SIZE,
                "Invalid REU write block"
            );
        }
        let host = match base.host() {
            Some(url::Host::Domain(name)) => name.to_owned(),
            Some(url::Host::Ipv4(address)) => address.to_string(),
            Some(url::Host::Ipv6(address)) => address.to_string(),
            None => bail!("C64U URL must contain a host"),
        };
        let port = self.dma_port;
        let mut stream =
            tokio::time::timeout(self.timeout, TcpStream::connect((host.as_str(), port)))
                .await
                .map_err(|_| {
                    anyhow!(
                        "Timed out connecting to the C64U DMA service (TCP port {port}); nothing was written"
                    )
                })?
                .map_err(|_| {
                    anyhow!(
                        "Could not connect to the C64U DMA service (TCP port {port}); enable the Ultimate DMA Service in the device's network settings. Nothing was written"
                    )
                })?;
        let _ = stream.set_nodelay(true);
        if let Some(password) = &self.password {
            let secret = password.as_bytes();
            ensure!(
                secret.len() <= usize::from(u16::MAX),
                "C64U_PASSWORD is too long for the DMA service"
            );
            let mut reply = [0u8];
            async {
                self.dma(
                    stream.write_all(&dma_frame(DMA_AUTHENTICATE, &[secret])),
                    Phase::Auth,
                    self.timeout,
                )
                .await?;
                self.dma(stream.read_exact(&mut reply), Phase::Auth, self.timeout)
                    .await
            }
            .await
            .context("DMA authentication failed; nothing was written")?;
            ensure!(
                reply[0] == 1,
                "DMA authentication refused; check C64U_PASSWORD. Nothing was written"
            );
        }
        // A busy or dead service fails here, before anything is written.
        self.identify(
            &mut stream,
            Phase::Probe,
            DMA_PROBE_TIMEOUT.min(self.timeout),
        )
        .await?;
        for (address, bytes) in writes {
            let frame = dma_frame(DMA_REU_WRITE, &[&address.to_le_bytes()[..3], bytes]);
            self.dma(stream.write_all(&frame), Phase::Write, self.timeout)
                .await?;
        }
        // REU writes have no reply. IDENTIFY does, and the device handles
        // commands in order, so its reply confirms that every write is done.
        self.identify(&mut stream, Phase::Identify, self.timeout)
            .await?;
        let _ = tokio::time::timeout(self.timeout, stream.shutdown()).await;
        Ok(())
    }

    /// Send IDENTIFY and read its length-prefixed reply.
    async fn identify(&self, stream: &mut TcpStream, phase: Phase, limit: Duration) -> Result<()> {
        self.dma(
            stream.write_all(&dma_frame(DMA_IDENTIFY, &[])),
            phase,
            limit,
        )
        .await?;
        let mut length = [0u8];
        self.dma(stream.read_exact(&mut length), phase, limit)
            .await?;
        let mut identity = vec![0u8; usize::from(length[0])];
        self.dma(stream.read_exact(&mut identity), phase, limit)
            .await?;
        Ok(())
    }

    /// Bound one socket operation and label its failure with the phase, so
    /// the error says whether anything may have been written and mentions
    /// the password only where it can be the cause.
    async fn dma<T>(
        &self,
        operation: impl Future<Output = std::io::Result<T>>,
        phase: Phase,
        limit: Duration,
    ) -> Result<T> {
        let failure = match tokio::time::timeout(limit, operation).await {
            Ok(Ok(value)) => return Ok(value),
            Err(_) => "timed out",
            Ok(Err(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                "the service closed the connection"
            }
            Ok(Err(_)) => "the connection failed",
        };
        let hint = match phase {
            Phase::Auth => " (check C64U_PASSWORD)",
            Phase::Probe if self.password.is_none() => {
                " (the service may be busy, or the device may require C64U_PASSWORD)"
            }
            Phase::Probe => " (the service may be busy)",
            Phase::Write | Phase::Identify => "",
        };
        let outcome = match phase {
            Phase::Auth | Phase::Probe => "nothing was written",
            Phase::Write | Phase::Identify => "part of the REU range may have been written",
        };
        bail!("C64U DMA {} {failure}{hint}; {outcome}", phase.name())
    }

    fn request(
        &self,
        method: Method,
        route: &str,
        query: &[(String, String)],
    ) -> Result<RequestBuilder> {
        let base = self.base.as_ref().ok_or_else(no_host)?;
        ensure!(
            !route.is_empty()
                && !route.starts_with('/')
                && !route.contains(['?', '#', '\\'])
                && !route.chars().any(char::is_control)
                && !route.split('/').any(|segment| {
                    let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
                    decoded == "." || decoded == ".."
                }),
            "Invalid device API route"
        );
        // Url::join interprets `machine:readmem` as a URL scheme. Append the
        // caller's already-encoded route to the normalized root instead.
        let mut url = Url::parse(&format!("{base}{route}"))
            .map_err(|_| anyhow!("Invalid device API route"))?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(k, v)| (k, v)));
        }
        Ok(self.client.request(method, url))
    }

    async fn send(&self, request: RequestBuilder) -> Result<Response> {
        request.send().await.map_err(transport_error)
    }

    async fn json_response(&self, request: RequestBuilder) -> Result<Value> {
        let response = self.send(request).await?;
        let status = response.status();
        let bytes = read_bounded(response).await?;
        self.decode_json(status, &bytes)
    }

    fn decode_json(&self, status: StatusCode, bytes: &[u8]) -> Result<Value> {
        let value = serde_json::from_slice::<Value>(bytes);
        if !status.is_success() {
            let detail = value
                .as_ref()
                .ok()
                .and_then(|value| self.device_errors(value));
            let context = match status {
                StatusCode::FORBIDDEN => "authentication refused; check C64U_PASSWORD",
                StatusCode::NOT_FOUND => "resource or endpoint unavailable on this firmware",
                StatusCode::NOT_IMPLEMENTED => "unsupported by this device or firmware",
                _ if status.is_redirection() => "redirect refused",
                _ => "request failed",
            };
            bail!(
                "Device HTTP {} ({context}){}",
                status.as_u16(),
                detail
                    .map(|message| format!(": {message}"))
                    .unwrap_or_default()
            );
        }
        let value = value
            .map_err(|_| anyhow!("Device returned malformed JSON (HTTP {})", status.as_u16()))?;
        ensure!(value.is_object(), "Device JSON response must be an object");
        if let Some(errors) = value.get("errors") {
            ensure!(
                errors
                    .as_array()
                    .is_some_and(|array| array.iter().all(Value::is_string)),
                "Device returned a malformed errors array"
            );
        }
        if let Some(message) = self.device_errors(&value) {
            bail!("Device reported an error: {message}");
        }
        Ok(value)
    }

    fn device_errors(&self, value: &Value) -> Option<String> {
        let errors = value.get("errors")?.as_array()?;
        if errors.is_empty() {
            return None;
        }
        let message = errors
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("; ");
        let message = if let Some(password) = &self.password {
            message.replace(password.as_ref(), "[REDACTED]")
        } else {
            message
        };
        // Device text is untrusted: keep terminal control characters out of
        // diagnostics and do not echo an arbitrarily large response.
        Some(
            message
                .chars()
                .map(|ch| if ch.is_control() { ' ' } else { ch })
                .take(2048)
                .collect(),
        )
    }
}

/// Steps of a DMA REU upload, for error messages.
#[derive(Clone, Copy)]
enum Phase {
    Auth,
    Probe,
    Write,
    Identify,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Auth => "authentication",
            Self::Probe => "probe",
            Self::Write => "write",
            Self::Identify => "final IDENTIFY",
        }
    }
}

fn no_host() -> anyhow::Error {
    anyhow!("No C64U host configured; set C64U_HOST or pass --host before using device tools")
}

/// A DMA command: little-endian command word and payload length, then payload.
fn dma_frame(command: u16, payload: &[&[u8]]) -> Vec<u8> {
    let length: usize = payload.iter().map(|part| part.len()).sum();
    debug_assert!(length <= usize::from(u16::MAX));
    let mut frame = Vec::with_capacity(4 + length);
    frame.extend_from_slice(&command.to_le_bytes());
    frame.extend_from_slice(&(length as u16).to_le_bytes());
    for part in payload {
        frame.extend_from_slice(part);
    }
    frame
}

fn normalize_host(host: &str) -> Result<Url> {
    let host = host.trim();
    ensure!(!host.is_empty(), "C64U host cannot be empty");
    let address = if host.contains("://") {
        host.to_owned()
    } else {
        format!("http://{host}")
    };
    let mut url =
        Url::parse(&address).map_err(|_| anyhow!("C64U host is not a valid URL or hostname"))?;
    ensure!(
        matches!(url.scheme(), "http" | "https"),
        "C64U URL must use HTTP or HTTPS"
    );
    ensure!(url.has_host(), "C64U URL must contain a host");
    let authority_has_userinfo = address.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    });
    ensure!(
        !authority_has_userinfo && url.username().is_empty() && url.password().is_none(),
        "C64U URL must not contain credentials; use C64U_PASSWORD"
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "C64U URL must not contain a query or fragment"
    );
    ensure!(
        matches!(url.path(), "/" | "/v1" | "/v1/"),
        "C64U URL must be a device root or /v1 API root"
    );
    url.set_path("/v1/");
    Ok(url)
}

fn check_size(size: usize, kind: &str) -> Result<()> {
    ensure!(
        size <= MAX_BODY_BYTES,
        "{kind} exceeds the 32 MiB body limit"
    );
    Ok(())
}

async fn read_bounded(mut response: Response) -> Result<Vec<u8>> {
    if let Some(length) = response.content_length() {
        ensure!(
            length <= MAX_BODY_BYTES as u64,
            "Response exceeds the 32 MiB body limit"
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let size = bytes
            .len()
            .checked_add(chunk.len())
            .ok_or_else(|| anyhow!("Response exceeds the 32 MiB body limit"))?;
        check_size(size, "Response")?;
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn transport_error(error: reqwest::Error) -> anyhow::Error {
    // reqwest errors may embed a URL containing query values. Do not surface it.
    if error.is_timeout() {
        anyhow!("Device request timed out; an action may have completed, so it was not retried")
    } else if error.is_connect() {
        anyhow!(
            "Could not connect to the configured C64U; check its address and Web Remote Control service"
        )
    } else {
        anyhow!(
            "Device connection failed or response was incomplete; an action may have completed, so it was not retried"
        )
    }
}
