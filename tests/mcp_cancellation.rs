//! Cancellation must travel through MCP, not merely abort a Rust helper future.
//! All HTTP and UDP traffic in this file stays on loopback.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequest, CallToolRequestParams, CallToolResult, ClientRequest},
    service::{PeerRequestOptions, RequestHandle, RunningService},
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::{process::Command, sync::Notify, time::timeout};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const WAIT: Duration = Duration::from_secs(5);

async fn start(mock: &MockServer) -> RunningService<RoleClient, ()> {
    let port = std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut command = Command::new(env!("CARGO_BIN_EXE_c64umcp"));
    command.kill_on_drop(true);
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
        "2",
        "--capture-ip",
        "127.0.0.1",
        "--capture-port",
        &port.to_string(),
        "--capture-timeout",
        "3",
    ]);
    // Keep test lock files out of the system temp directory.
    command.args(["--lock-dir", env!("CARGO_TARGET_TMPDIR")]);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    timeout(WAIT, ().serve(TokioChildProcess::new(command).unwrap()))
        .await
        .unwrap()
        .unwrap()
}

fn params(name: &str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_owned()).with_arguments(args.as_object().unwrap().clone())
}

async fn send(
    client: &RunningService<RoleClient, ()>,
    name: &str,
    args: Value,
) -> RequestHandle<RoleClient> {
    client
        .send_cancellable_request(
            ClientRequest::CallToolRequest(CallToolRequest::new(params(name, args))),
            PeerRequestOptions::no_options(),
        )
        .await
        .unwrap()
}

async fn info(client: &RunningService<RoleClient, ()>) -> CallToolResult {
    timeout(WAIT, client.call_tool(params("get_info", json!({}))))
        .await
        .expect("device lock remained held after cancellation cleanup")
        .unwrap()
}

async fn mock_info(mock: &MockServer) -> Arc<Mutex<Option<Instant>>> {
    let received = Arc::new(Mutex::new(None));
    let record_time = received.clone();
    Mock::given(method("GET"))
        .and(path("/v1/info"))
        .respond_with(move |_: &wiremock::Request| {
            *record_time.lock().unwrap() = Some(Instant::now());
            ResponseTemplate::new(200).set_body_json(json!({"product":"loopback", "errors":[]}))
        })
        .expect(1)
        .mount(mock)
        .await;
    received
}

#[tokio::test]
async fn stdio_cancellation_stops_typing_and_preserves_cleanup_order() {
    let mock = MockServer::start().await;
    let pressed = Arc::new(Notify::new());
    let released = Arc::new(Notify::new());
    let release_started = Arc::new(Mutex::new(None::<Instant>));
    let press_signal = pressed.clone();
    let release_signal = released.clone();
    let release_time = release_started.clone();
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .respond_with(move |request: &wiremock::Request| {
            let event = &request.body_json::<Value>().unwrap()["events"][0];
            let response = ResponseTemplate::new(200).set_body_json(json!({"errors":[]}));
            if event["transition"] == "press" {
                press_signal.notify_one();
                response
            } else {
                *release_time.lock().unwrap() = Some(Instant::now());
                release_signal.notify_one();
                response.set_delay(Duration::from_millis(200))
            }
        })
        .expect(2)
        .mount(&mock)
        .await;
    let info_received = mock_info(&mock).await;
    let client = start(&mock).await;
    let typing = send(
        &client,
        "type_text",
        json!({"text":"abcdefgh", "hold_ms":1000, "gap_ms":1}),
    )
    .await;
    timeout(WAIT, pressed.notified()).await.unwrap();
    typing
        .cancel(Some("test cancelled typing".into()))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), released.notified())
        .await
        .expect("no cancellation key cleanup");
    let result = timeout(Duration::from_secs(2), info(&client))
        .await
        .expect("cancelled typing kept executing");
    assert_eq!(result.is_error, Some(false));
    assert!(
        info_received
            .lock()
            .unwrap()
            .unwrap()
            .duration_since(release_started.lock().unwrap().unwrap())
            >= Duration::from_millis(180)
    );
    let requests = mock.received_requests().await.unwrap();
    let events: Vec<Value> = requests
        .iter()
        .filter(|request| request.url.path() == "/v1/machine:input")
        .map(|request| request.body_json::<Value>().unwrap()["events"][0].clone())
        .collect();
    assert_eq!(
        events,
        vec![
            json!({"kind":"keyboard","inputs":["a"],"transition":"press"}),
            json!({"kind":"keyboard","inputs":["a"],"transition":"release"}),
        ]
    );
    assert_eq!(requests.last().unwrap().url.path(), "/v1/info");
    timeout(WAIT, client.cancel()).await.unwrap().unwrap();
    mock.verify().await;
}

#[tokio::test]
async fn cancelled_queued_mutation_never_reaches_device() {
    let mock = MockServer::start().await;
    let pressed = Arc::new(Notify::new());
    let press_signal = pressed.clone();
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .respond_with(move |request: &wiremock::Request| {
            if request.body_json::<Value>().unwrap()["events"][0]["transition"] == "press" {
                press_signal.notify_one();
            }
            ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
        })
        .expect(2)
        .mount(&mock)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/machine:writemem"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errors":[]})))
        .expect(0)
        .mount(&mock)
        .await;
    mock_info(&mock).await;
    let client = start(&mock).await;
    let typing = send(
        &client,
        "type_text",
        json!({"text":"a", "hold_ms":1000, "gap_ms":1}),
    )
    .await;
    timeout(WAIT, pressed.notified()).await.unwrap();
    let queued = send(
        &client,
        "write_memory",
        json!({"address":"0400", "data_hex":"01"}),
    )
    .await;
    // A protocol response is a barrier after the queued tool request; tool
    // discovery itself does not acquire the device lock.
    timeout(WAIT, client.list_all_tools())
        .await
        .unwrap()
        .unwrap();
    queued
        .cancel(Some("cancel queued mutation".into()))
        .await
        .unwrap();
    timeout(WAIT, client.list_all_tools())
        .await
        .unwrap()
        .unwrap();
    timeout(WAIT, typing.await_response())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(info(&client).await.is_error, Some(false));
    assert!(
        mock.received_requests()
            .await
            .unwrap()
            .iter()
            .all(|request| !request.url.path().ends_with("writemem"))
    );
    timeout(WAIT, client.cancel()).await.unwrap().unwrap();
    mock.verify().await;
}

async fn capture_cancel_case(stop_status: u16) {
    let mock = MockServer::start().await;
    let started = Arc::new(Notify::new());
    let stopped = Arc::new(Notify::new());
    let stop_started = Arc::new(Mutex::new(None::<Instant>));
    let start_signal = started.clone();
    Mock::given(method("PUT"))
        .and(path("/v1/streams/video:start"))
        .respond_with(move |_: &wiremock::Request| {
            start_signal.notify_one();
            ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
        })
        .expect(1)
        .mount(&mock)
        .await;
    let stop_signal = stopped.clone();
    let stop_time = stop_started.clone();
    Mock::given(method("PUT")).and(path("/v1/streams/video:stop"))
        .respond_with(move |_: &wiremock::Request| {
            *stop_time.lock().unwrap() = Some(Instant::now());
            stop_signal.notify_one();
            ResponseTemplate::new(stop_status).set_body_json(json!({"errors":
                if stop_status == 200 { Vec::<String>::new() } else { vec!["injected stop failure".into()] }
            })).set_delay(Duration::from_millis(200))
        }).expect(1).mount(&mock).await;
    let info_received = mock_info(&mock).await;
    let client = start(&mock).await;
    let capture = send(&client, "capture_screenshot", json!({})).await;
    timeout(WAIT, started.notified()).await.unwrap();
    capture
        .cancel(Some("cancel screenshot".into()))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), stopped.notified())
        .await
        .expect("no screenshot cancellation cleanup");
    assert_eq!(info(&client).await.is_error, Some(false));
    assert!(
        info_received
            .lock()
            .unwrap()
            .unwrap()
            .duration_since(stop_started.lock().unwrap().unwrap())
            >= Duration::from_millis(180)
    );
    let requests = mock.received_requests().await.unwrap();
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/v1/streams/video:start",
            "/v1/streams/video:stop",
            "/v1/info"
        ]
    );
    timeout(WAIT, client.cancel()).await.unwrap().unwrap();
    mock.verify().await;
}

#[tokio::test]
async fn cancelled_capture_stops_before_subsequent_device_call() {
    capture_cancel_case(200).await;
}

#[tokio::test]
async fn cancelled_capture_cleanup_failure_still_releases_device_lock() {
    capture_cancel_case(500).await;
}
