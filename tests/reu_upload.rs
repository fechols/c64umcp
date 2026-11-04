//! upload_reu contracts: wiremock REST plus a loopback stand-in for the DMA
//! service socket. No configured device is contacted.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use c64umcp::{device::Device, server::C64Server};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest},
    service::{PeerRequestOptions, RequestHandle, RunningService},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[derive(Debug, Clone, PartialEq)]
enum Event {
    Rest(&'static str),
    Connect,
    Auth(Vec<u8>),
    Write(u32, Vec<u8>),
    Identify,
    Unknown(u16),
}

type Log = Arc<Mutex<Vec<Event>>>;

fn push(log: &Log, event: Event) {
    log.lock().unwrap().push(event);
}

#[derive(Clone, Copy)]
enum Dma {
    Normal,
    RefuseAuth,
    CloseOnAuth,
    CloseAfterWrites(usize),
    /// Answers the IDENTIFY probe sent before the writes, not the final one.
    NoFinalIdentifyReply,
    /// Accepts the connection but never answers, like a busy service.
    NoProbeReply,
    Closed,
}

/// Accepts DMA connections, records each command and answers the commands
/// that have replies, like the Ultimate firmware's DMA service.
async fn fake_dma(log: Log, behavior: Dma) -> (u16, Option<JoinHandle<()>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    if let Dma::Closed = behavior {
        return (port, None);
    }
    let task = tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            push(&log, Event::Connect);
            let mut writes = 0;
            let mut identifies = 0;
            loop {
                let mut header = [0u8; 4];
                if stream.read_exact(&mut header).await.is_err() {
                    break;
                }
                let command = u16::from_le_bytes([header[0], header[1]]);
                let mut payload =
                    vec![0u8; usize::from(u16::from_le_bytes([header[2], header[3]]))];
                if stream.read_exact(&mut payload).await.is_err() {
                    break;
                }
                match command {
                    0xff1f => {
                        push(&log, Event::Auth(payload));
                        if matches!(behavior, Dma::CloseOnAuth) {
                            break;
                        }
                        let refuse = matches!(behavior, Dma::RefuseAuth);
                        let _ = stream.write_all(&[if refuse { 0 } else { 1 }]).await;
                    }
                    0xff07 if payload.len() > 3 => {
                        let address = u32::from_le_bytes([payload[0], payload[1], payload[2], 0]);
                        push(&log, Event::Write(address, payload[3..].to_vec()));
                        writes += 1;
                        if matches!(behavior, Dma::CloseAfterWrites(n) if n == writes) {
                            break;
                        }
                    }
                    0xff0e if payload.is_empty() => {
                        push(&log, Event::Identify);
                        identifies += 1;
                        let silent = match behavior {
                            Dma::NoProbeReply => true,
                            Dma::NoFinalIdentifyReply => identifies > 1,
                            _ => false,
                        };
                        if !silent {
                            let _ = stream.write_all(b"\x0bULTIMATE-II").await;
                        }
                    }
                    other => push(&log, Event::Unknown(other)),
                }
            }
        }
    });
    (port, Some(task))
}

struct Harness {
    http: MockServer,
    log: Log,
    dma_port: u16,
    client: RunningService<RoleClient, ()>,
    server: JoinHandle<()>,
    dma: Option<JoinHandle<()>>,
}

impl Harness {
    async fn new(password: Option<&str>, behavior: Dma) -> Self {
        Self::with_lock_dir(password, behavior, None).await
    }

    async fn with_lock_dir(password: Option<&str>, behavior: Dma, lock_dir: Option<&Path>) -> Self {
        let http = MockServer::start().await;
        let log = Log::default();
        let (dma_port, dma) = fake_dma(log.clone(), behavior).await;
        let (client, server) = connect(&http.uri(), password, dma_port, lock_dir).await;
        Self {
            http,
            log,
            dma_port,
            client,
            server,
            dma,
        }
    }

    /// Mount a REST route that records its call in the event log.
    async fn rest(&self, verb: &str, route: &str, name: &'static str, status: u16, times: u64) {
        let log = self.log.clone();
        Mock::given(method(verb))
            .and(path(format!("/v1/{route}")))
            .respond_with(move |_: &wiremock::Request| {
                push(&log, Event::Rest(name));
                if status == 200 {
                    ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
                } else {
                    ResponseTemplate::new(status).set_body_json(json!({"errors":["test failure"]}))
                }
            })
            .expect(times)
            .mount(&self.http)
            .await;
    }

    async fn reset(&self, status: u16, times: u64) {
        self.rest("PUT", "machine:reset", "reset", status, times)
            .await;
    }

    async fn machine(&self, pauses: u64, resumes: u64) {
        self.rest("PUT", "machine:pause", "pause", 200, pauses)
            .await;
        self.rest("PUT", "machine:resume", "resume", 200, resumes)
            .await;
    }

    async fn call(&self, args: Value) -> CallToolResult {
        tokio::time::timeout(
            Duration::from_secs(10),
            self.client.call_tool(
                CallToolRequestParams::new("upload_reu")
                    .with_arguments(args.as_object().unwrap().clone()),
            ),
        )
        .await
        .expect("MCP call timed out")
        .expect("MCP protocol failure")
    }

    async fn success(&self, args: Value) -> Value {
        let result = self.call(args).await;
        assert_ne!(result.is_error, Some(true), "{:?}", result.content);
        result.structured_content.expect("structured tool result")
    }

    async fn failure(&self, args: Value, message: &str) {
        let result = self.call(args).await;
        assert_eq!(result.is_error, Some(true), "upload_reu should fail");
        let text = serde_json::to_string(&result.content).unwrap();
        assert!(text.contains(message), "{text}");
    }

    /// Start a call that the test can cancel through MCP.
    async fn send(&self, args: Value) -> RequestHandle<RoleClient> {
        self.client
            .send_cancellable_request(
                ClientRequest::CallToolRequest(CallToolRequest::new(
                    CallToolRequestParams::new("upload_reu")
                        .with_arguments(args.as_object().unwrap().clone()),
                )),
                PeerRequestOptions::no_options(),
            )
            .await
            .unwrap()
    }

    async fn wait_until(&self, done: impl Fn(&[Event]) -> bool) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !done(&self.events()) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("condition never met: {:?}", self.events()));
    }

    async fn wait_for(&self, event: Event) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !self.events().contains(&event) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{event:?} never happened: {:?}", self.events()));
    }

    fn events(&self) -> Vec<Event> {
        self.log.lock().unwrap().clone()
    }

    async fn finish(mut self) {
        self.http.verify().await;
        self.client.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.server)
            .await
            .unwrap()
            .unwrap();
        if let Some(dma) = self.dma {
            dma.abort();
        }
    }
}

/// Serve one MCP server for the device at `uri`, as a separate C64Server.
async fn connect(
    uri: &str,
    password: Option<&str>,
    dma_port: u16,
    lock_dir: Option<&Path>,
) -> (RunningService<RoleClient, ()>, JoinHandle<()>) {
    let device = Device::new(Some(uri), password, Duration::from_secs(2))
        .unwrap()
        .with_dma_port(dma_port);
    let mut server = C64Server::new(device, None, 0, Duration::from_secs(2));
    if let Some(dir) = lock_dir {
        server = server.with_lock_dir(dir);
    }
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    let server = tokio::spawn(async move {
        server
            .serve(server_transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let client = ().serve(client_transport).await.unwrap();
    (client, server)
}

struct TemporaryFile(PathBuf);
impl TemporaryFile {
    fn new(bytes: &[u8]) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "c64umcp-reu-{}-{}.bin",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
    fn source(&self) -> Value {
        json!({"kind":"local","path":self.0})
    }
}
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn pattern(length: usize) -> Vec<u8> {
    (0..length).map(|i| (i * 7 + i / 251) as u8).collect()
}

fn writes(events: &[Event]) -> Vec<(u32, usize)> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::Write(address, bytes) => Some((*address, bytes.len())),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn guarded_upload_resets_zeroes_guard_writes_it_last_then_runs() {
    let h = Harness::new(None, Dma::Normal).await;
    h.reset(200, 1).await;
    h.machine(0, 0).await;
    h.rest("POST", "runners:run_prg", "run_prg", 200, 1).await;
    let data = pattern(70000);
    let mut file = vec![0xAA; 100];
    file.extend_from_slice(&data);
    let file = TemporaryFile::new(&file);
    let prg = [0x01, 0x08, 0x60];
    let result = h
        .success(json!({
            "source": file.source(),
            "offset": 100,
            "address": "$560000",
            "guard": {"address":"560000","length":16},
            "run_prg": {"kind":"base64","filename":"start.prg","data":STANDARD.encode(prg)},
        }))
        .await;
    assert_eq!(
        result,
        json!({
            "address":"560000",
            "bytes_written":70000,
            "chunks":4,
            "guard":{"address":"560000","length":16,"zero_chunks":1},
            "reset":true,
            "paused":false,
            "ran":true,
        })
    );
    let events = h.events();
    assert_eq!(events.first(), Some(&Event::Rest("reset")));
    assert_eq!(events[1], Event::Connect);
    assert_eq!(events[2], Event::Identify);
    assert_eq!(events[3], Event::Write(0x560000, vec![0; 16]));
    assert_eq!(
        writes(&events),
        [
            (0x560000, 16),
            (0x560010, 32768),
            (0x568010, 32768),
            (0x570010, 4448),
            (0x560000, 16),
        ]
    );
    assert_eq!(
        &events[events.len() - 2..],
        [Event::Identify, Event::Rest("run_prg")]
    );
    // Reassemble the REU image from the data writes: it must equal the slice.
    let mut image = vec![0xEE; data.len()];
    for event in &events[3..] {
        if let Event::Write(address, bytes) = event {
            let start = (*address - 0x560000) as usize;
            image[start..start + bytes.len()].copy_from_slice(bytes);
        }
    }
    assert_eq!(image, data);
    assert_eq!(
        events[events.len() - 3],
        Event::Write(0x560000, data[..16].to_vec())
    );
    let requests = h.http.received_requests().await.unwrap();
    assert!(requests[1].body.windows(3).any(|window| window == prg));
    h.finish().await;
}

#[tokio::test]
async fn unpaused_upload_chunks_at_32_kib_with_explicit_length_and_no_rest_calls() {
    let h = Harness::new(None, Dma::Normal).await;
    h.machine(0, 0).await;
    let data = pattern(65537);
    let result = h
        .success(json!({
            "source": {"kind":"base64","filename":"image.reu","data":STANDARD.encode(&data)},
            "offset": 1,
            "length": 65536,
            "address": "0xFF0000",
            "reset": false,
            "pause": false,
        }))
        .await;
    assert_eq!(result["chunks"], 2);
    assert_eq!(result["reset"], false);
    assert_eq!(result["paused"], false);
    assert_eq!(result["ran"], false);
    assert_eq!(result["guard"], Value::Null);
    let events = h.events();
    assert_eq!(
        events,
        [
            Event::Connect,
            Event::Identify,
            Event::Write(0xFF0000, data[1..32769].to_vec()),
            Event::Write(0xFF8000, data[32769..].to_vec()),
            Event::Identify,
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn failed_reset_connects_to_nothing_and_never_pauses() {
    let h = Harness::new(None, Dma::Normal).await;
    h.reset(500, 1).await;
    h.machine(0, 0).await;
    h.rest("POST", "runners:run_prg", "run_prg", 200, 0).await;
    h.failure(
        json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
            "address": "0",
            "pause": true,
            "run_prg": {"kind":"base64","filename":"start.prg","data":STANDARD.encode([1, 8, 0x60])},
        }),
        "could not reset the C64; nothing was written",
    )
    .await;
    assert_eq!(h.events(), [Event::Rest("reset")]);
    h.finish().await;
}

#[tokio::test]
async fn reset_and_pause_together_reset_first() {
    let h = Harness::new(None, Dma::Normal).await;
    h.reset(200, 1).await;
    h.machine(1, 1).await;
    let result = h
        .success(json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1, 2])},
            "address": "0",
            "pause": true,
        }))
        .await;
    assert_eq!(result["reset"], true);
    assert_eq!(result["paused"], true);
    assert_eq!(
        h.events(),
        [
            Event::Rest("reset"),
            Event::Rest("pause"),
            Event::Connect,
            Event::Identify,
            Event::Write(0, vec![1, 2]),
            Event::Identify,
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn password_authenticates_the_dma_socket_before_any_write() {
    let h = Harness::new(Some("offline-secret"), Dma::Normal).await;
    h.machine(1, 1).await;
    h.success(json!({
        "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1, 2, 3])},
        "address": "0",
        "reset": false,
    }))
    .await;
    assert_eq!(
        h.events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Auth(b"offline-secret".to_vec()),
            Event::Identify,
            Event::Write(0, vec![1, 2, 3]),
            Event::Identify,
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn a_failed_resume_is_reported_and_retried() {
    let h = Harness::new(None, Dma::Normal).await;
    h.rest("PUT", "machine:pause", "pause", 200, 1).await;
    let log = h.log.clone();
    let resumes = Arc::new(AtomicU64::new(0));
    Mock::given(method("PUT"))
        .and(path("/v1/machine:resume"))
        .respond_with(move |_: &wiremock::Request| {
            push(&log, Event::Rest("resume"));
            // Only the first attempt fails.
            if resumes.fetch_add(1, Ordering::SeqCst) == 0 {
                ResponseTemplate::new(500).set_body_json(json!({"errors":["test failure"]}))
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
            }
        })
        .expect(2)
        .mount(&h.http)
        .await;
    h.failure(
        json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1, 2, 3])},
            "address": "0",
            "reset": false,
        }),
        "resuming the C64 failed",
    )
    .await;
    // The armed guard retries once after the failed resume.
    h.wait_until(|events| {
        events
            .iter()
            .filter(|event| **event == Event::Rest("resume"))
            .count()
            == 2
    })
    .await;
    h.finish().await;
}

#[tokio::test]
async fn refused_authentication_writes_nothing_and_still_resumes() {
    let h = Harness::new(Some("wrong"), Dma::RefuseAuth).await;
    h.machine(1, 1).await;
    h.rest("POST", "runners:run_prg", "run_prg", 200, 0).await;
    h.failure(
        json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1, 2, 3])},
            "address": "0",
            "reset": false,
            "run_prg": {"kind":"base64","filename":"start.prg","data":STANDARD.encode([1, 8, 0x60])},
        }),
        "DMA authentication refused",
    )
    .await;
    assert_eq!(
        h.events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Auth(b"wrong".to_vec()),
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn socket_failures_still_resume_and_skip_run_prg() {
    for (behavior, message) in [
        // Windows retries a refused loopback connect until the timeout.
        (Dma::Closed, "the C64 was resumed: "),
        (Dma::CloseAfterWrites(1), "the C64 was resumed"),
    ] {
        let h = Harness::new(None, behavior).await;
        h.machine(1, 1).await;
        h.rest("POST", "runners:run_prg", "run_prg", 200, 0).await;
        h.failure(
            json!({
                "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode(pattern(200000))},
                "address": "100000",
                "reset": false,
                "run_prg": {"kind":"base64","filename":"start.prg","data":STANDARD.encode([1, 8, 0x60])},
            }),
            message,
        )
        .await;
        let events = h.events();
        assert_eq!(events.first(), Some(&Event::Rest("pause")));
        assert_eq!(events.last(), Some(&Event::Rest("resume")));
        // At most the probe: the final IDENTIFY is never reached.
        assert!(events.iter().filter(|e| **e == Event::Identify).count() <= 1);
        h.finish().await;
    }
}

#[tokio::test]
async fn failed_pause_writes_nothing() {
    let h = Harness::new(None, Dma::Normal).await;
    h.rest("PUT", "machine:pause", "pause", 500, 1).await;
    // A failed pause may still have paused the CPU, so it is resumed anyway.
    h.rest("PUT", "machine:resume", "resume", 200, 1).await;
    h.failure(
        json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
            "address": "0",
            "reset": false,
        }),
        "could not pause the C64; nothing was written",
    )
    .await;
    assert_eq!(h.events(), [Event::Rest("pause"), Event::Rest("resume")]);
    h.finish().await;
}

#[tokio::test]
async fn dropped_authentication_does_not_claim_a_partial_write() {
    let h = Harness::new(Some("secret"), Dma::CloseOnAuth).await;
    h.machine(1, 1).await;
    let result = h
        .call(json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
            "address": "0",
            "reset": false,
        }))
        .await;
    assert_eq!(result.is_error, Some(true));
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(
        text.contains("DMA authentication failed; nothing was written"),
        "{text}"
    );
    assert!(!text.contains("may have been written"), "{text}");
    assert_eq!(
        h.events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Auth(b"secret".to_vec()),
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn cancelled_pause_request_still_resumes() {
    let h = Harness::new(None, Dma::Normal).await;
    let log = h.log.clone();
    Mock::given(method("PUT"))
        .and(path("/v1/machine:pause"))
        .respond_with(move |_: &wiremock::Request| {
            push(&log, Event::Rest("pause"));
            ResponseTemplate::new(200)
                .set_body_json(json!({"errors":[]}))
                .set_delay(Duration::from_secs(1))
        })
        .expect(1)
        .mount(&h.http)
        .await;
    h.rest("PUT", "machine:resume", "resume", 200, 1).await;
    let call = h
        .send(json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
            "address": "0",
            "reset": false,
        }))
        .await;
    h.wait_for(Event::Rest("pause")).await;
    call.cancel(Some("cancel while pausing".into()))
        .await
        .unwrap();
    h.wait_for(Event::Rest("resume")).await;
    assert_eq!(h.events(), [Event::Rest("pause"), Event::Rest("resume")]);
    h.finish().await;
}

#[tokio::test]
async fn cancelled_transfer_still_resumes() {
    let h = Harness::new(None, Dma::NoFinalIdentifyReply).await;
    h.machine(1, 1).await;
    let call = h
        .send(json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1, 2])},
            "address": "0",
            "reset": false,
        }))
        .await;
    h.wait_for(Event::Write(0, vec![1, 2])).await;
    h.wait_until(|events| events.iter().filter(|e| **e == Event::Identify).count() == 2)
        .await;
    call.cancel(Some("cancel during transfer".into()))
        .await
        .unwrap();
    h.wait_for(Event::Rest("resume")).await;
    assert_eq!(
        h.events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Identify,
            Event::Write(0, vec![1, 2]),
            Event::Identify,
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn a_silent_dma_service_fails_the_probe_before_any_write() {
    let h = Harness::new(None, Dma::NoProbeReply).await;
    h.machine(1, 1).await;
    h.rest("POST", "runners:run_prg", "run_prg", 200, 0).await;
    let result = h
        .call(json!({
            "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
            "address": "0",
            "reset": false,
            "run_prg": {"kind":"base64","filename":"start.prg","data":STANDARD.encode([1, 8, 0x60])},
        }))
        .await;
    assert_eq!(result.is_error, Some(true));
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(text.contains("C64U DMA probe timed out"), "{text}");
    assert!(text.contains("nothing was written"), "{text}");
    assert!(!text.contains("may have been written"), "{text}");
    assert_eq!(
        h.events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Identify,
            Event::Rest("resume"),
        ]
    );
    h.finish().await;
}

#[tokio::test]
async fn shutdown_resumes_a_paused_upload_it_could_not_join() {
    let http = MockServer::start().await;
    let log = Log::default();
    let (dma_port, dma) = fake_dma(log.clone(), Dma::NoFinalIdentifyReply).await;
    for (route, name) in [("machine:pause", "pause"), ("machine:resume", "resume")] {
        let log = log.clone();
        Mock::given(method("PUT"))
            .and(path(format!("/v1/{route}")))
            .respond_with(move |_: &wiremock::Request| {
                push(&log, Event::Rest(name));
                ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
            })
            .mount(&http)
            .await;
    }
    // A long device timeout keeps the upload stuck on its final IDENTIFY.
    let device = Device::new(Some(&http.uri()), None, Duration::from_secs(30))
        .unwrap()
        .with_dma_port(dma_port);
    let server = C64Server::new(device, None, 0, Duration::from_secs(2))
        .with_shutdown_join_timeout(Duration::from_millis(300));
    let shutdown = server.clone();
    let (server_transport, client_transport) = tokio::io::duplex(1 << 20);
    // serve waits for the client's initialize, so run it as its own task.
    let running = tokio::spawn(async move { server.serve(server_transport).await.unwrap() });
    let client = ().serve(client_transport).await.unwrap();
    let running = running.await.unwrap();
    let _call = client
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(
                CallToolRequestParams::new("upload_reu").with_arguments(
                    json!({
                        "source": {"kind":"base64","filename":"x.bin","data":STANDARD.encode([1])},
                        "address": "0",
                        "reset": false,
                    })
                    .as_object()
                    .unwrap()
                    .clone(),
                ),
            )),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap();
    let events = || log.lock().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(5), async {
        while events().iter().filter(|e| **e == Event::Identify).count() < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    // The handler is still running, so shutdown cannot join it.
    let started = std::time::Instant::now();
    tokio::time::timeout(Duration::from_secs(5), shutdown.finish_shutdown())
        .await
        .expect("shutdown must stay bounded");
    assert!(started.elapsed() < Duration::from_secs(4));
    assert_eq!(
        events(),
        [
            Event::Rest("pause"),
            Event::Connect,
            Event::Identify,
            Event::Write(0, vec![1]),
            Event::Identify,
            Event::Rest("resume"),
        ]
    );
    drop(client);
    drop(running);
    if let Some(dma) = dma {
        dma.abort();
    }
}

#[tokio::test]
async fn invalid_ranges_and_sources_never_reach_the_device() {
    let h = Harness::new(None, Dma::Normal).await;
    h.machine(0, 0).await;
    let two = STANDARD.encode([1, 2]);
    let base64 = |data: &str| json!({"kind":"base64","filename":"x.bin","data":data});
    for (args, message) in [
        (
            json!({"source":base64(&two),"address":"FFFFFF"}),
            "may not pass $FFFFFF",
        ),
        (
            json!({"source":base64(&two),"address":"1000000"}),
            "24-bit hexadecimal",
        ),
        (
            json!({"source":base64(&two),"address":"$12G"}),
            "24-bit hexadecimal",
        ),
        (
            json!({"source":base64(&two),"address":"0","offset":2}),
            "offset must lie within",
        ),
        (
            json!({"source":base64(&two),"address":"0","offset":1,"length":2}),
            "fit in the file after offset",
        ),
        (
            json!({"source":base64(&two),"address":"0","length":0}),
            "must be nonzero",
        ),
        (
            json!({"source":base64(&two),"address":"10","guard":{"address":"F","length":1}}),
            "guard range must lie inside",
        ),
        (
            json!({"source":base64(&two),"address":"10","guard":{"address":"11","length":2}}),
            "guard range must lie inside",
        ),
        (
            json!({"source":base64(&two),"address":"10","guard":{"address":"10","length":0}}),
            "must be nonempty",
        ),
        (
            json!({"source":{"kind":"device","path":"/Usb0/image.reu"},"address":"0"}),
            "local or base64 source",
        ),
        (
            json!({"source":{"kind":"local","path":"image.reu"},"address":"0"}),
            "must be absolute",
        ),
        (
            json!({"source":base64(&two),"address":"0","run_prg":base64(&STANDARD.encode([0xff, 0xff, 1, 2]))}),
            "exceeds the address space",
        ),
    ] {
        h.failure(args, message).await;
    }
    let extra = h
        .call(json!({"source":base64(&two),"address":"0","bank":1}))
        .await;
    assert_eq!(extra.is_error, Some(true));
    assert!(h.events().is_empty(), "{:?}", h.events());
    h.finish().await;
}

#[tokio::test]
async fn servers_sharing_a_device_lock_never_interleave_uploads() {
    let lock_dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("reu-locks-{}", std::process::id()));
    let h = Harness::with_lock_dir(None, Dma::Normal, Some(&lock_dir)).await;
    let log = h.log.clone();
    // A slow reset keeps the first upload inside the lock while the second starts.
    Mock::given(method("PUT"))
        .and(path("/v1/machine:reset"))
        .respond_with(move |_: &wiremock::Request| {
            push(&log, Event::Rest("reset"));
            ResponseTemplate::new(200)
                .set_body_json(json!({"errors":[]}))
                .set_delay(Duration::from_millis(400))
        })
        .expect(2)
        .mount(&h.http)
        .await;
    let (mut other, other_server) = connect(&h.http.uri(), None, h.dma_port, Some(&lock_dir)).await;
    let first = h
        .send(json!({
            "source": {"kind":"base64","filename":"a.bin","data":STANDARD.encode([1])},
            "address": "0",
        }))
        .await;
    h.wait_for(Event::Rest("reset")).await;
    let second = tokio::time::timeout(
        Duration::from_secs(10),
        other.call_tool(
            CallToolRequestParams::new("upload_reu").with_arguments(
                json!({
                    "source": {"kind":"base64","filename":"b.bin","data":STANDARD.encode([2])},
                    "address": "0",
                })
                .as_object()
                .unwrap()
                .clone(),
            ),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(second.is_error, Some(true), "{:?}", second.content);
    first.await_response().await.unwrap();
    assert_eq!(
        h.events(),
        [
            Event::Rest("reset"),
            Event::Connect,
            Event::Identify,
            Event::Write(0, vec![1]),
            Event::Identify,
            Event::Rest("reset"),
            Event::Connect,
            Event::Identify,
            Event::Write(0, vec![2]),
            Event::Identify,
        ]
    );
    other.close().await.unwrap();
    other_server.await.unwrap();
    h.finish().await;
}
