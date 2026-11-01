//! Contract tests use loopback only; no configured device is contacted.

use std::{collections::HashMap, time::Duration};

use c64umcp::device::{Device, MAX_BODY_BYTES};
use reqwest::Method;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::oneshot,
    task::JoinHandle,
};

struct Request {
    line: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

struct Mock {
    address: String,
    request: Option<oneshot::Receiver<Request>>,
    task: JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn raw(response: Vec<u8>, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut received = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "request closed before headers");
                received.extend_from_slice(&chunk[..count]);
                if let Some(index) = received.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                    break index + 4;
                }
            };
            let text = String::from_utf8(received[..header_end].to_vec()).unwrap();
            let mut lines = text.lines();
            let line = lines.next().unwrap().to_owned();
            let headers: HashMap<_, _> = lines
                .filter_map(|line| line.split_once(':'))
                .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
                .collect();
            let length = headers
                .get("content-length")
                .map(|length| length.parse::<usize>().unwrap())
                .unwrap_or(0);
            while received.len() - header_end < length {
                let mut chunk = [0u8; 4096];
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "request closed before body");
                received.extend_from_slice(&chunk[..count]);
            }
            let _ = tx.send(Request {
                line,
                headers,
                body: received[header_end..].to_vec(),
            });
            tokio::time::sleep(delay).await;
            let _ = stream.write_all(&response).await;
            let _ = stream.shutdown().await;
        });
        Self {
            address,
            request: Some(rx),
            task,
        }
    }

    async fn response(status: u16, content_type: &str, body: impl AsRef<[u8]>) -> Self {
        let body = body.as_ref();
        let mut response = format!(
            "HTTP/1.1 {status} Test\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        ).into_bytes();
        response.extend_from_slice(body);
        Self::raw(response, Duration::ZERO).await
    }

    fn device(&self) -> Device {
        Device::new(Some(&self.address), None, Duration::from_secs(2)).unwrap()
    }

    async fn received(&mut self) -> Request {
        tokio::time::timeout(Duration::from_secs(2), self.request.take().unwrap())
            .await
            .unwrap()
            .unwrap()
    }
}

#[test]
fn normalize_host_and_reject_credential_or_path_injection() {
    for (host, expected) in [
        ("c64u", "http://c64u/v1/"),
        ("localhost:8080/", "http://localhost:8080/v1/"),
        ("https://c64u/v1", "https://c64u/v1/"),
        ("http://[::1]:8000/v1/", "http://[::1]:8000/v1/"),
    ] {
        assert_eq!(
            Device::new(Some(host), None, Duration::from_secs(1))
                .unwrap()
                .base_url()
                .unwrap()
                .as_str(),
            expected
        );
    }
    for host in [
        "",
        "http://user:secret@c64u",
        "http://@c64u",
        "http://c64u?token=secret",
        "http://c64u/#fragment",
        "ftp://c64u",
        "http://c64u/other",
    ] {
        assert!(
            Device::new(Some(host), None, Duration::from_secs(1)).is_err(),
            "{host}"
        );
    }
    assert!(
        Device::new(
            Some("c64u"),
            Some("secret\r\ninjected: yes"),
            Duration::from_secs(1)
        )
        .is_err()
    );
    assert!(Device::new(None, None, Duration::ZERO).is_err());
}

#[tokio::test]
async fn missing_host_is_allowed_until_a_request() {
    let device = Device::new(None, None, Duration::from_secs(1)).unwrap();
    assert!(device.base_url().is_none());
    assert!(
        device
            .json(Method::GET, "info", &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("No C64U host configured")
    );
}

#[tokio::test]
async fn encoded_paths_query_values_and_password_reach_the_device() {
    let mut mock = Mock::response(200, "application/json", br#"{"errors":[],"current":8}"#).await;
    let device = Device::new(
        Some(&format!("{}/v1", mock.address)),
        Some("a-secret"),
        Duration::from_secs(2),
    )
    .unwrap();
    let value = device
        .json(
            Method::PUT,
            "configs/Drive%20A%20Settings/Drive%20Bus%20ID",
            &[("value".into(), "a+b &?#/%".into())],
            None,
        )
        .await
        .unwrap();
    assert_eq!(value["current"], 8);
    let request = mock.received().await;
    assert_eq!(
        request.line,
        "PUT /v1/configs/Drive%20A%20Settings/Drive%20Bus%20ID?value=a%2Bb+%26%3F%23%2F%25 HTTP/1.1"
    );
    assert_eq!(request.headers["x-password"], "a-secret");
}

#[tokio::test]
async fn json_body_preserves_nested_types() {
    let mut mock = Mock::response(200, "application/json", br#"{"errors":[]}"#).await;
    let body = json!({"Drive A Settings": {"Drive Bus ID": 9, "Drive": "Enabled"}});
    mock.device()
        .json(Method::POST, "configs", &[], Some(body.clone()))
        .await
        .unwrap();
    let request = mock.received().await;
    assert_eq!(request.line, "POST /v1/configs HTTP/1.1");
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        body
    );
}

#[tokio::test]
async fn uploads_preserve_binary_data_names_and_attachment_order() {
    let mut mock = Mock::response(200, "application/json", br#"{"errors":[]}"#).await;
    mock.device()
        .upload(
            "runners:sidplay",
            &[("songnr".into(), "2".into())],
            vec![
                ("song.sid".into(), vec![0, 255, 1, 2]),
                ("song.ssl".into(), b"1:23".to_vec()),
            ],
        )
        .await
        .unwrap();
    let request = mock.received().await;
    assert_eq!(request.line, "POST /v1/runners:sidplay?songnr=2 HTTP/1.1");
    assert!(request.headers["content-type"].starts_with("multipart/form-data; boundary="));
    let text = String::from_utf8_lossy(&request.body);
    assert!(
        text.find("filename=\"song.sid\"").unwrap() < text.find("filename=\"song.ssl\"").unwrap()
    );
    assert!(request.body.windows(4).any(|bytes| bytes == [0, 255, 1, 2]));
    assert!(text.contains("1:23"));
}

#[tokio::test]
async fn raw_upload_and_binary_read_are_lossless() {
    let bytes = vec![0, 255, 10, 13, 128];
    let mut upload = Mock::response(200, "application/json", br#"{"errors":[]}"#).await;
    upload
        .device()
        .write_bytes(
            "machine:writemem",
            &[("address".into(), "C000".into())],
            bytes.clone(),
        )
        .await
        .unwrap();
    let request = upload.received().await;
    assert_eq!(
        request.line,
        "POST /v1/machine:writemem?address=C000 HTTP/1.1"
    );
    assert_eq!(request.headers["content-type"], "application/octet-stream");
    assert_eq!(request.body, bytes);
    let download = Mock::response(200, "application/octet-stream", &bytes).await;
    assert_eq!(
        download
            .device()
            .binary("machine:readmem", &[])
            .await
            .unwrap(),
        bytes
    );
    // Arbitrary memory can contain JSON text; an explicit binary content type
    // must preserve those bytes instead of interpreting them as an API envelope.
    let memory = br#"{"errors":["this is memory, not an API error"]}"#;
    let download = Mock::response(200, "application/octet-stream", memory).await;
    assert_eq!(
        download
            .device()
            .binary("machine:readmem", &[])
            .await
            .unwrap(),
        memory
    );
}

#[tokio::test]
async fn errors_are_detected_and_secrets_redacted() {
    for status in [200, 400, 403, 404, 501] {
        let mock = Mock::response(
            status,
            "application/json",
            br#"{"errors":["a-secret rejected"]}"#,
        )
        .await;
        let device = Device::new(
            Some(&mock.address),
            Some("a-secret"),
            Duration::from_secs(2),
        )
        .unwrap();
        let error = device
            .json(Method::GET, "info", &[], None)
            .await
            .unwrap_err()
            .to_string();
        assert!(!error.contains("a-secret"));
        assert!(error.contains("[REDACTED] rejected"));
        if status != 200 {
            assert!(error.contains(&status.to_string()));
        }
    }
}

#[tokio::test]
async fn malformed_json_and_error_arrays_are_rejected() {
    for body in ["invalid", "[]", r#"{"errors":null}"#, r#"{"errors":[42]}"#] {
        let mock = Mock::response(200, "application/json", body).await;
        assert!(
            mock.device()
                .json(Method::GET, "info", &[], None)
                .await
                .is_err()
        );
    }
    for (status, body) in [
        (200, "invalid"),
        (200, r#"{"errors":[]}"#),
        (200, r#"{"errors":["wrong"]}"#),
        (404, r#"{"errors":["missing"]}"#),
    ] {
        let mock = Mock::response(status, "application/json", body).await;
        assert!(mock.device().binary("machine:readmem", &[]).await.is_err());
    }
}

#[tokio::test]
async fn binary_errors_without_content_type_are_still_errors() {
    let body = br#"{"errors":["wrong address"]}"#;
    let response = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
    let mut response = response.into_bytes();
    response.extend_from_slice(body);
    let mock = Mock::raw(response, Duration::ZERO).await;
    assert!(
        mock.device()
            .binary("machine:readmem", &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("wrong address")
    );
}

#[tokio::test]
async fn redirect_is_refused_without_sending_the_password_to_the_target() {
    let mut target = Mock::response(200, "application/json", br#"{"errors":[]}"#).await;
    let response = format!(
        "HTTP/1.1 302 Found\r\nLocation: {}/v1/info\r\nContent-Length: 0\r\n\r\n",
        target.address
    );
    let redirect = Mock::raw(response.into_bytes(), Duration::ZERO).await;
    let device = Device::new(
        Some(&redirect.address),
        Some("secret"),
        Duration::from_secs(1),
    )
    .unwrap();
    assert!(
        device
            .json(Method::GET, "info", &[], None)
            .await
            .unwrap_err()
            .to_string()
            .contains("redirect refused")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), target.request.take().unwrap())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn timeout_and_short_response_do_not_leak_urls_or_request_values() {
    let mock = Mock::raw(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}".to_vec(),
        Duration::from_secs(1),
    )
    .await;
    let device = Device::new(Some(&mock.address), None, Duration::from_millis(25)).unwrap();
    let error = device
        .json(
            Method::PUT,
            "configs/item",
            &[("value".into(), "private-value".into())],
            None,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("timed out"));
    assert!(!error.contains("private-value"));
    assert!(!error.contains(&mock.address));
    let short = Mock::raw(
        b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_vec(),
        Duration::ZERO,
    )
    .await;
    assert!(
        short
            .device()
            .binary("machine:readmem", &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("incomplete")
    );
}

#[tokio::test]
async fn a_disconnected_mutation_is_never_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        drop(stream);
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok()
    });
    let device = Device::new(Some(&address), None, Duration::from_secs(1)).unwrap();
    let error = device
        .json(Method::PUT, "machine:reset", &[], None)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("not retried"));
    assert!(
        !server.await.unwrap(),
        "client retried an uncertain mutation"
    );
}

#[tokio::test]
async fn response_limits_apply_with_and_without_content_length() {
    let declared = Mock::raw(
        format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_BODY_BYTES + 1
        )
        .into_bytes(),
        Duration::ZERO,
    )
    .await;
    assert!(
        declared
            .device()
            .binary("machine:readmem", &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("32 MiB")
    );
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
        MAX_BODY_BYTES + 1
    )
    .into_bytes();
    response.resize(response.len() + MAX_BODY_BYTES + 1, 0x55);
    response.extend_from_slice(b"\r\n0\r\n\r\n");
    let streamed = Mock::raw(response, Duration::ZERO).await;
    assert!(
        streamed
            .device()
            .binary("machine:readmem", &[])
            .await
            .unwrap_err()
            .to_string()
            .contains("32 MiB")
    );
}

#[tokio::test]
async fn upload_limits_and_route_validation_run_before_network_access() {
    let device = Device::new(Some("http://127.0.0.1:1"), None, Duration::from_secs(1)).unwrap();
    assert!(
        device
            .write_bytes("runners:run_prg", &[], vec![0; MAX_BODY_BYTES + 1])
            .await
            .unwrap_err()
            .to_string()
            .contains("32 MiB")
    );
    assert!(
        device
            .upload(
                "runners:run_prg",
                &[],
                vec![("x.prg".into(), vec![0; MAX_BODY_BYTES])]
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("32 MiB")
    );
    for route in [
        "",
        "/info",
        "../info",
        "%2e%2e/info",
        "info?x=1",
        "info#x",
        "files\\other",
    ] {
        assert_eq!(
            device
                .json(Method::GET, route, &[], None)
                .await
                .unwrap_err()
                .to_string(),
            "Invalid device API route"
        );
    }
}
