//! End-to-end MCP checks against the actual executable and loopback-only devices.
use std::time::Duration;

use base64::{Engine, engine::general_purpose::STANDARD};

use rmcp::{
    RoleClient, ServiceExt, model::CallToolRequestParams, service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use tokio::{process::Command, time::timeout};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

async fn start_server(
    host: Option<&str>,
    password: Option<&str>,
) -> RunningService<RoleClient, ()> {
    start_server_with_capture(host, password, None).await
}

async fn start_server_with_capture(
    host: Option<&str>,
    password: Option<&str>,
    capture_port: Option<u16>,
) -> RunningService<RoleClient, ()> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_c64umcp"));
    command.kill_on_drop(true);
    // A developer's real device configuration must never leak into offline tests.
    for variable in [
        "C64U_HOST",
        "C64U_PASSWORD",
        "C64U_TIMEOUT",
        "C64U_CAPTURE_IP",
        "C64U_CAPTURE_PORT",
        "C64U_CAPTURE_TIMEOUT",
        "C64U_LOCK_DIR",
        "RUST_LOG",
    ] {
        command.env_remove(variable);
    }
    if let Some(host) = host {
        command.args(["--host", host]);
    }
    if let Some(password) = password {
        command.env("C64U_PASSWORD", password);
    }
    if let Some(port) = capture_port {
        command.args([
            "--capture-ip",
            "127.0.0.1",
            "--capture-port",
            &port.to_string(),
        ]);
    }
    // Keep test lock files out of the system temp directory.
    command.args(["--lock-dir", env!("CARGO_TARGET_TMPDIR")]);
    #[cfg(windows)]
    command.creation_flags(0x08000000); // CREATE_NO_WINDOW
    let transport = TokioChildProcess::new(command).expect("spawn MCP executable");
    timeout(TEST_TIMEOUT, ().serve(transport))
        .await
        .expect("MCP initialization timed out")
        .expect("MCP initialization failed")
}

fn call(name: &str, arguments: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_owned())
        .with_arguments(arguments.as_object().expect("object arguments").clone())
}

#[tokio::test]
async fn screenshot_round_trip_returns_png_image_and_stops_stream() {
    use wiremock::matchers::query_param;
    let port = {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        socket.local_addr().unwrap().port()
    };
    let device = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path("/v1/streams/video:start"))
        .and(query_param("ip", format!("127.0.0.1:{port}")))
        .respond_with(move |_: &wiremock::Request| {
            tokio::spawn(async move {
                let sender = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
                // The capture drops a frame that began before the start
                // response arrived, so send a few.
                for frame in 42..45u16 {
                    for packet_number in 0..60u16 {
                        let mut packet = vec![0x12; 780];
                        let line =
                            (packet_number * 4) | if packet_number == 59 { 0x8000 } else { 0 };
                        packet[0..2].copy_from_slice(&packet_number.to_le_bytes());
                        packet[2..4].copy_from_slice(&frame.to_le_bytes());
                        packet[4..6].copy_from_slice(&line.to_le_bytes());
                        packet[6..8].copy_from_slice(&384u16.to_le_bytes());
                        packet[8] = 4;
                        packet[9] = 4;
                        packet[10..12].fill(0);
                        sender
                            .send_to(&packet, (std::net::Ipv4Addr::LOCALHOST, port))
                            .await
                            .unwrap();
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
            });
            ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
        })
        .expect(1)
        .mount(&device)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/streams/video:stop"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"errors":[]})))
        .expect(1)
        .mount(&device)
        .await;
    let client = start_server_with_capture(Some(&device.uri()), None, Some(port)).await;
    let result = timeout(
        TEST_TIMEOUT,
        client.call_tool(call("capture_screenshot", json!({}))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(result.is_error, Some(true), "capture failed: {result:?}");
    let image = result
        .content
        .iter()
        .find_map(|content| content.as_image())
        .expect("image content");
    assert_eq!(image.mime_type, "image/png");
    let bytes = STANDARD.decode(&image.data).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    let reader = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    assert_eq!(reader.info().width, 384);
    assert_eq!(reader.info().height, 240);
    timeout(TEST_TIMEOUT, client.cancel())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn executable_negotiates_discovers_and_calls_device_with_structured_content() {
    let device = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/version"))
        .and(header("X-Password", "offline-test-password"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"version":"0.1", "errors":[]})),
        )
        .expect(1)
        .mount(&device)
        .await;
    let client = start_server(Some(&device.uri()), Some("offline-test-password")).await;
    let peer = client.peer_info().expect("server info");
    let identity = peer
        .server_info
        .as_ref()
        .expect("server implementation identity");
    assert_eq!(identity.name, "c64umcp");
    assert_eq!(identity.version, env!("CARGO_PKG_VERSION"));
    assert!(
        client
            .peer_info()
            .expect("server info")
            .capabilities
            .tools
            .is_some()
    );
    let tools = timeout(TEST_TIMEOUT, client.list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(tools.len(), 48, "expected complete tool catalog");
    for name in [
        "get_version",
        "get_info",
        "read_memory",
        "run_prg",
        "upload_reu",
        "send_input",
        "capture_screenshot",
        "claim_device",
        "release_device",
        "device_lock_status",
    ] {
        let tool = tools
            .iter()
            .find(|tool| tool.name == name)
            .unwrap_or_else(|| panic!("missing tool {name}"));
        assert_eq!(tool.input_schema.get("type"), Some(&json!("object")));
    }
    let result = timeout(
        TEST_TIMEOUT,
        client.call_tool(call("get_version", json!({}))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_ne!(result.is_error, Some(true));
    assert_eq!(
        result.structured_content.as_ref().unwrap()["version"],
        "0.1"
    );
    assert!(
        result
            .content
            .iter()
            .any(|content| content.as_text().is_some())
    );
    timeout(TEST_TIMEOUT, client.cancel())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn device_errors_are_tool_errors_and_invalid_arguments_never_reach_device() {
    let device = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/info"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"errors":["unsupported test hardware"]})),
        )
        .expect(1)
        .mount(&device)
        .await;
    let client = start_server(Some(&device.uri()), None).await;
    let failed = timeout(TEST_TIMEOUT, client.call_tool(call("get_info", json!({}))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(failed.is_error, Some(true));
    assert!(
        failed
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .any(|text| text.text.contains("unsupported test hardware"))
    );

    // The SDK surfaces typed argument-deserialization failures as tool errors.
    let malformed = timeout(
        TEST_TIMEOUT,
        client.call_tool(call("read_memory", json!({"address":42}))),
    )
    .await
    .unwrap();
    assert_eq!(malformed.unwrap().is_error, Some(true));
    // Invalid ranges are semantic tool errors and must also stay local.
    let out_of_range = timeout(
        TEST_TIMEOUT,
        client.call_tool(call("read_memory", json!({"address":"FFFF", "length":2}))),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(out_of_range.is_error, Some(true));
    let extra_field = timeout(
        TEST_TIMEOUT,
        client.call_tool(call(
            "send_input",
            json!({"events":[{"kind":"release_all", "inputs":["a"]}]}),
        )),
    )
    .await
    .unwrap();
    assert_eq!(extra_field.unwrap().is_error, Some(true));
    assert_eq!(device.received_requests().await.unwrap().len(), 1);
    timeout(TEST_TIMEOUT, client.cancel())
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn executable_without_host_still_supports_discovery_and_reports_configuration_error() {
    let client = start_server(None, None).await;
    let tools = timeout(TEST_TIMEOUT, client.list_all_tools())
        .await
        .unwrap()
        .unwrap();
    assert!(tools.iter().any(|tool| tool.name == "get_info"));
    let result = timeout(TEST_TIMEOUT, client.call_tool(call("get_info", json!({}))))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.is_error, Some(true));
    assert!(
        result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .any(|text| text.text.contains("C64U_HOST"))
    );
    timeout(TEST_TIMEOUT, client.cancel())
        .await
        .unwrap()
        .unwrap();
}
