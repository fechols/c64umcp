//! A shutdown signal (Ctrl-Break on Windows, SIGINT on Unix) must run the
//! same bounded device cleanup as a closed stdin, and a second signal must
//! exit at once.
//!
//! On Windows the server runs in its own process group, because
//! GenerateConsoleCtrlEvent can target a group only with CTRL_BREAK_EVENT;
//! Ctrl-C and Ctrl-Break take the same path in the server.
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

struct Server {
    child: Child,
    // Held open: the signal, not stdin EOF, must end the server.
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
}

impl Server {
    async fn start(mock: &MockServer) -> Self {
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
            "--lock-dir",
            env!("CARGO_TARGET_TMPDIR"),
        ]);
        // CREATE_NEW_PROCESS_GROUP; the child shares this console so that a
        // console control event can reach it.
        #[cfg(windows)]
        command.creation_flags(0x0000_0200);
        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
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
                    "clientInfo":{"name":"signal-regression", "version":"1"}
                }
            }))
            .await;
        server.response(1).await;
        server
            .send(json!({"jsonrpc":"2.0", "method":"notifications/initialized"}))
            .await;
        server
    }

    async fn send(&mut self, message: Value) {
        let mut bytes = serde_json::to_vec(&message).unwrap();
        bytes.push(b'\n');
        self.stdin.write_all(&bytes).await.unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn response(&mut self, id: u64) -> Value {
        let line = timeout(WAIT, self.stdout.next_line())
            .await
            .expect("MCP response timed out")
            .unwrap()
            .expect("MCP exited early");
        let response: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(response["id"], id, "{response}");
        response
    }

    async fn press_fire(&mut self) {
        self.send(json!({
            "jsonrpc":"2.0", "id":2, "method":"tools/call",
            "params":{"name":"send_input","arguments":{"events":[
                {"kind":"joystick","port":2,"inputs":["fire"],"transition":"press"}
            ]}}
        }))
        .await;
        let response = self.response(2).await;
        assert_ne!(response["result"]["isError"], true, "{response}");
    }

    fn signal(&self) {
        let pid = self.child.id().expect("server is running");
        #[cfg(windows)]
        {
            unsafe extern "system" {
                fn GenerateConsoleCtrlEvent(event: u32, process_group: u32) -> i32;
            }
            const CTRL_BREAK_EVENT: u32 = 1;
            // SAFETY: a plain Win32 call with integer arguments.
            let sent = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
            assert_ne!(
                sent,
                0,
                "GenerateConsoleCtrlEvent failed: {}",
                std::io::Error::last_os_error()
            );
        }
        #[cfg(unix)]
        {
            let status = std::process::Command::new("kill")
                .args(["-INT", &pid.to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }
    }

    async fn exit_code(&mut self) -> Option<i32> {
        timeout(WAIT, self.child.wait())
            .await
            .expect("server did not exit after the signal")
            .unwrap()
            .code()
    }
}

#[tokio::test]
async fn signal_releases_held_inputs_and_exits_nonzero() {
    let mock = MockServer::start().await;
    let released = Arc::new(Mutex::new(Vec::new()));
    let record = released.clone();
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request.body_json::<Value>().unwrap();
            if body["events"][0]["transition"] == "release" {
                record.lock().unwrap().push(body);
            }
            ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
        })
        .expect(2)
        .mount(&mock)
        .await;
    let mut server = Server::start(&mock).await;
    server.press_fire().await;
    let signalled = Instant::now();
    server.signal();
    let code = server.exit_code().await;
    assert!(
        signalled.elapsed() < Duration::from_secs(4),
        "shutdown took {:?}",
        signalled.elapsed()
    );
    assert_eq!(code, Some(130));
    assert_eq!(
        *released.lock().unwrap(),
        [json!({"events":[{"kind":"joystick","port":2,"inputs":["fire"],"transition":"release"}]})]
    );
    mock.verify().await;
}

#[tokio::test]
async fn second_signal_exits_without_waiting_for_cleanup() {
    let mock = MockServer::start().await;
    let releasing = Arc::new(Notify::new());
    let signal = releasing.clone();
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .respond_with(move |request: &wiremock::Request| {
            let body = request.body_json::<Value>().unwrap();
            let response = ResponseTemplate::new(200).set_body_json(json!({"errors":[]}));
            if body["events"][0]["transition"] == "release" {
                signal.notify_one();
                // Cleanup would wait out its three-second bound.
                response.set_delay(Duration::from_secs(30))
            } else {
                response
            }
        })
        .expect(2)
        .mount(&mock)
        .await;
    let mut server = Server::start(&mock).await;
    server.press_fire().await;
    server.signal();
    timeout(WAIT, releasing.notified()).await.unwrap();
    let second = Instant::now();
    server.signal();
    server.exit_code().await;
    assert!(
        second.elapsed() < Duration::from_millis(1500),
        "second signal took {:?} to exit",
        second.elapsed()
    );
    mock.verify().await;
}
