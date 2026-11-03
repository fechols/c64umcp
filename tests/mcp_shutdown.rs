//! A normal MCP stdin EOF must allow bounded device cleanup before process exit.
//! Raw pipes deliberately avoid the client's automatic cancellation on drop.
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Notify,
    time::timeout,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const WAIT: Duration = Duration::from_secs(15);
const RESPONSE_DELAY: Duration = Duration::from_millis(500);

struct StdioServer {
    child: Child,
    stdin: Option<ChildStdin>,
    // Keep the read end open after stdin EOF, so this tests orderly shutdown
    // rather than an unrelated broken stdout pipe.
    stdout: Lines<BufReader<ChildStdout>>,
}

impl StdioServer {
    async fn start(mock: &MockServer) -> Self {
        let port = std::net::UdpSocket::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let mut command = Command::new(env!("CARGO_BIN_EXE_c64umcp"));
        command
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for name in [
            "C64U_HOST",
            "C64U_PASSWORD",
            "C64U_TIMEOUT",
            "C64U_CAPTURE_IP",
            "C64U_CAPTURE_PORT",
            "C64U_CAPTURE_TIMEOUT",
            "C64U_LOCK_DIR",
            "RUST_LOG",
        ] {
            command.env_remove(name);
        }
        command.args([
            "--host",
            &mock.uri(),
            "--timeout",
            "60",
            "--capture-ip",
            "127.0.0.1",
            "--capture-port",
            &port.to_string(),
            "--capture-timeout",
            "60",
        ]);
        // Keep test lock files out of the system temp directory.
        command.args(["--lock-dir", env!("CARGO_TARGET_TMPDIR")]);
        #[cfg(windows)]
        command.creation_flags(0x08000000);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut server = Self {
            child,
            stdin,
            stdout,
        };
        server
            .send(json!({
                "jsonrpc":"2.0", "id":1, "method":"initialize",
                "params":{
                    "protocolVersion":"2025-03-26", "capabilities":{},
                    "clientInfo":{"name":"shutdown-regression", "version":"1"}
                }
            }))
            .await;
        let line = timeout(WAIT, server.stdout.next_line())
            .await
            .expect("MCP initialization timed out")
            .unwrap()
            .expect("MCP exited before initialization");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], 1);
        assert!(response.get("result").is_some(), "{response}");
        server
            .send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await;
        server
    }

    async fn send(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.stdin
            .as_mut()
            .unwrap()
            .write_all(&bytes)
            .await
            .unwrap();
        self.stdin.as_mut().unwrap().flush().await.unwrap();
    }

    async fn call(&mut self, name: &str, arguments: Value) {
        self.send(json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }))
        .await;
    }

    async fn close_stdin_and_wait(&mut self) -> Instant {
        drop(self.stdin.take());
        let status = timeout(WAIT, self.child.wait())
            .await
            .expect("MCP shutdown exceeded its bounded cleanup window")
            .unwrap();
        let exited = Instant::now();
        assert!(status.success(), "MCP exited with {status}");
        exited
    }
}

async fn capture_eof_case(stop_delay: Duration) -> (Instant, Instant) {
    let mock = MockServer::start().await;
    let started = Arc::new(Notify::new());
    let signal = started.clone();
    Mock::given(method("PUT"))
        .and(path("/v1/streams/video:start"))
        .respond_with(move |_: &wiremock::Request| {
            signal.notify_one();
            ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let stop_at = Arc::new(Mutex::new(None));
    let record = stop_at.clone();
    Mock::given(method("PUT"))
        .and(path("/v1/streams/video:stop"))
        .respond_with(move |_: &wiremock::Request| {
            *record.lock().unwrap() = Some(Instant::now());
            ResponseTemplate::new(200)
                .set_body_json(json!({"errors":[]}))
                .set_delay(stop_delay)
        })
        .expect(1)
        .mount(&mock)
        .await;
    let mut server = StdioServer::start(&mock).await;
    server.call("capture_screenshot", json!({})).await;
    timeout(WAIT, started.notified()).await.unwrap();
    let exited = server.close_stdin_and_wait().await;
    let stopped = stop_at
        .lock()
        .unwrap()
        .expect("normal stdin EOF exited without stopping the video stream");
    mock.verify().await;
    (stopped, exited)
}

#[tokio::test]
async fn stdin_eof_waits_for_active_capture_cleanup() {
    let (stopped, exited) = capture_eof_case(RESPONSE_DELAY).await;
    assert!(
        exited.duration_since(stopped) >= RESPONSE_DELAY,
        "process exited before the delayed stop response completed"
    );
}

#[tokio::test]
async fn stdin_eof_bounds_unresponsive_capture_cleanup() {
    let (stopped, exited) = capture_eof_case(Duration::from_secs(30)).await;
    assert!(
        exited.duration_since(stopped) >= Duration::from_millis(2500),
        "shutdown abandoned cleanup instead of awaiting its deadline"
    );
    assert!(
        exited.duration_since(stopped) < Duration::from_secs(6),
        "shutdown used the HTTP timeout instead of the cleanup deadline"
    );
}

#[tokio::test]
async fn stdin_eof_releases_inflight_key_press_before_exit() {
    let mock = MockServer::start().await;
    let pressed = Arc::new(Notify::new());
    let signal = pressed.clone();
    let release_at = Arc::new(Mutex::new(None));
    let record = release_at.clone();
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .respond_with(move |request: &wiremock::Request| {
            let event = &request.body_json::<Value>().unwrap()["events"][0];
            let response = ResponseTemplate::new(200).set_body_json(json!({"errors":[]}));
            if event["transition"] == "press" {
                signal.notify_one();
                // The device may apply a press before its HTTP response reaches
                // the client. Hold that response so EOF interrupts this state.
                response.set_delay(Duration::from_secs(30))
            } else {
                assert_eq!(event["transition"], "release");
                assert_eq!(event["inputs"], json!(["a"]));
                *record.lock().unwrap() = Some(Instant::now());
                response.set_delay(RESPONSE_DELAY)
            }
        })
        .expect(2)
        .mount(&mock)
        .await;
    let mut server = StdioServer::start(&mock).await;
    server
        .call("type_text", json!({"text":"ab", "hold_ms":1,"gap_ms":1}))
        .await;
    timeout(WAIT, pressed.notified()).await.unwrap();
    let exited = server.close_stdin_and_wait().await;
    let released = release_at
        .lock()
        .unwrap()
        .expect("normal stdin EOF exited without releasing the active key chord");
    assert!(
        exited.duration_since(released) >= RESPONSE_DELAY,
        "process exited before the delayed key release response completed"
    );
    mock.verify().await;
}
