//! End-to-end tool contracts: MCP duplex transport to a loopback HTTP device.

use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use base64::{Engine, engine::general_purpose::STANDARD};
use c64umcp::{device::Device, server::C64Server};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, CallToolResult},
    service::RunningService,
};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_bytes, body_json, method, path, query_param},
};

struct Harness {
    http: MockServer,
    client: RunningService<RoleClient, ()>,
    server: JoinHandle<()>,
}

impl Harness {
    async fn new() -> Self {
        Self::with_capture(None, Duration::from_secs(2)).await
    }

    async fn with_capture(capture_ip: Option<std::net::Ipv4Addr>, capture: Duration) -> Self {
        let http = MockServer::start().await;
        let device = Device::new(Some(&http.uri()), None, Duration::from_secs(2)).unwrap();
        let server = C64Server::new(device, capture_ip, 0, capture);
        let (server_transport, client_transport) = tokio::io::duplex(65536);
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
        Self {
            http,
            client,
            server,
        }
    }

    async fn call(&self, name: &str, args: Value) -> CallToolResult {
        tokio::time::timeout(
            Duration::from_secs(5),
            self.client.call_tool(
                CallToolRequestParams::new(name.to_owned())
                    .with_arguments(args.as_object().unwrap().clone()),
            ),
        )
        .await
        .expect("MCP call timed out")
        .expect("MCP protocol failure")
    }

    async fn success(&self, name: &str, args: Value) -> Value {
        let result = self.call(name, args).await;
        assert_ne!(result.is_error, Some(true), "{name}: {:?}", result.content);
        result.structured_content.expect("structured tool result")
    }

    async fn failure(&self, name: &str, args: Value, message: &str) {
        let result = self.call(name, args).await;
        assert_eq!(result.is_error, Some(true), "{name} should fail");
        assert!(
            serde_json::to_string(&result.content)
                .unwrap()
                .contains(message),
            "{name}: {:?}",
            result.content
        );
    }

    async fn finish(mut self) {
        self.http.verify().await;
        self.client.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), self.server)
            .await
            .unwrap()
            .unwrap();
    }
}

fn okay() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({"errors":[]}))
}

fn upload(name: &str, bytes: &[u8]) -> Value {
    json!({"kind":"base64", "filename":name, "data":STANDARD.encode(bytes)})
}

struct TemporaryFile(PathBuf);
impl TemporaryFile {
    fn new(bytes: &[u8]) -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "c64umcp-contract-{}-{}.crt",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, bytes).unwrap();
        Self(path)
    }
}
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[tokio::test]
async fn runners_distinguish_device_base64_local_and_sid_second_attachment() {
    let h = Harness::new().await;
    Mock::given(method("PUT"))
        .and(path("/v1/runners:run_prg"))
        .and(query_param("file", "/Usb0/My Programs/demo.prg"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    for route in ["load_prg", "run_crt", "sidplay"] {
        Mock::given(method("POST"))
            .and(path(format!("/v1/runners:{route}")))
            .respond_with(okay())
            .expect(1)
            .mount(&h.http)
            .await;
    }
    h.success(
        "run_prg",
        json!({"source":{"kind":"device","path":"/Usb0/My Programs/demo.prg"}}),
    )
    .await;
    let prg = [0x01, 0x08, 0x60];
    h.success("load_prg", json!({"source":upload("demo.prg", &prg)}))
        .await;
    let local = TemporaryFile::new(b"C64 CARTRIDGE\0fixture");
    h.success("run_crt", json!({"source":{"kind":"local","path":local.0}}))
        .await;
    h.success("play_sid", json!({"source":upload("tune.sid", b"PSID\0fixture"), "songnr":3, "song_lengths":upload("tune.ssl", b"0:42")})).await;
    let requests = h.http.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    assert!(
        requests[1]
            .body
            .windows(prg.len())
            .any(|window| window == prg)
    );
    assert!(
        requests[2]
            .body
            .windows(b"C64 CARTRIDGE\0".len())
            .any(|window| window == b"C64 CARTRIDGE\0")
    );
    let body = String::from_utf8_lossy(&requests[3].body);
    assert!(
        body.find("filename=\"tune.sid\"").unwrap() < body.find("filename=\"tune.ssl\"").unwrap()
    );
    assert!(body.contains("0:42"));
    assert_eq!(
        requests[3]
            .url
            .query_pairs()
            .find(|(key, _)| key == "songnr")
            .unwrap()
            .1,
        "3"
    );
    h.failure(
        "run_prg",
        json!({"source":upload("bad.prg", &[0xff,0xff,1,2])}),
        "exceeds the address space",
    )
    .await;
    h.failure(
        "run_crt",
        json!({"source":upload("cart.crt", b"fixture"),"songnr":1}),
        "song options only apply",
    )
    .await;
    assert_eq!(h.http.received_requests().await.unwrap().len(), 4);
    h.finish().await;
}

#[tokio::test]
async fn mounting_and_drive_rom_uploads_use_distinct_routes_and_validate_sizes() {
    let h = Harness::new().await;
    Mock::given(method("PUT"))
        .and(path("/v1/drives/a:mount"))
        .and(query_param("image", "/Usb0/disk one.d64"))
        .and(query_param("mode", "readonly"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/drives/b:mount"))
        .and(query_param("type", "d81"))
        .and(query_param("mode", "unlinked"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/drives/a:load_rom"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    h.success(
        "mount_disk",
        json!({"drive":"a","source":{"kind":"device","path":"/Usb0/disk one.d64"}}),
    )
    .await;
    h.success("mount_disk", json!({"drive":"b","source":upload("disk.d81", &[1,2,3]),"image_type":"d81","mode":"unlinked"})).await;
    h.failure(
        "load_drive_rom",
        json!({"drive":"a","source":upload("bad.rom", &[0;16])}),
        "16 or 32 KiB",
    )
    .await;
    h.success(
        "load_drive_rom",
        json!({"drive":"a","source":upload("drive.rom", &vec![0x55;16384])}),
    )
    .await;
    let requests = h.http.received_requests().await.unwrap();
    assert_eq!(requests.len(), 3);
    assert!(String::from_utf8_lossy(&requests[1].body).contains("filename=\"disk.d81\""));
    assert!(String::from_utf8_lossy(&requests[2].body).contains("filename=\"drive.rom\""));
    h.finish().await;
}

#[tokio::test]
async fn configuration_patterns_and_batch_types_survive_mcp_mapping() {
    let h = Harness::new().await;
    Mock::given(method("GET"))
        .and(path("/v1/configs/Drive%20A*/*Bus%20ID"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            json!({"Drive A Settings":{"Drive Bus ID":{"current":8,"min":8,"max":11}},"errors":[]}),
        ))
        .expect(1)
        .mount(&h.http)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/configs/Drive%20A*/*Bus%20ID"))
        .and(query_param("value", "9"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    let settings = json!({"Drive A Settings":{"Drive Bus ID":9,"Drive":"Enabled"}});
    Mock::given(method("POST"))
        .and(path("/v1/configs"))
        .and(body_json(&settings))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    let value = h
        .success(
            "get_config",
            json!({"category":"Drive A*","item":"*Bus ID"}),
        )
        .await;
    assert_eq!(value["Drive A Settings"]["Drive Bus ID"]["current"], 8);
    h.success(
        "set_config",
        json!({"category":"Drive A*","item":"*Bus ID","value":9}),
    )
    .await;
    h.success("set_config_batch", json!({"settings":settings}))
        .await;
    for settings in [
        json!({}),
        json!({"Drive A Settings":true}),
        json!({"Drive A Settings":{"Drive":true}}),
    ] {
        let outcome = h
            .call("set_config_batch", json!({"settings":settings}))
            .await;
        assert_eq!(outcome.is_error, Some(true));
    }
    assert_eq!(h.http.received_requests().await.unwrap().len(), 3);
    h.finish().await;
}

#[tokio::test]
async fn memory_write_switches_at_128_bytes_and_rejects_wrapping_ranges() {
    let h = Harness::new().await;
    Mock::given(method("PUT"))
        .and(path("/v1/machine:writemem"))
        .and(query_param("address", "C000"))
        .and(query_param("data", "AB".repeat(128)))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/machine:writemem"))
        .and(query_param("address", "C000"))
        .and(body_bytes(vec![0xab; 129]))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    h.success(
        "write_memory",
        json!({"address":"$C000","data_hex":"ab".repeat(128)}),
    )
    .await;
    h.success(
        "write_memory",
        json!({"address":"0xc000","data_hex":"ab".repeat(129)}),
    )
    .await;
    h.failure(
        "write_memory",
        json!({"address":"FFFF","data_hex":"0001"}),
        "may not pass $FFFF",
    )
    .await;
    h.failure(
        "write_memory",
        json!({"address":"C000","data_hex":"0"}),
        "pairs of hexadecimal",
    )
    .await;
    h.failure(
        "write_memory",
        json!({"address":"C000","data_hex":""}),
        "nonempty",
    )
    .await;
    assert_eq!(h.http.received_requests().await.unwrap().len(), 2);
    h.finish().await;
}

#[tokio::test]
async fn memory_reads_check_actual_length_and_return_lossless_encodings() {
    let h = Harness::new().await;
    for (address, bytes) in [
        ("C000", vec![0, 1, 2]),
        ("C100", vec![0, 1, 2, 3, 4]),
        ("C200", vec![0, 1, 0x80, 0xff]),
    ] {
        Mock::given(method("GET"))
            .and(path("/v1/machine:readmem"))
            .and(query_param("address", address))
            .and(query_param("length", "4"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes)
                    .insert_header("Content-Type", "application/octet-stream"),
            )
            .expect(1)
            .mount(&h.http)
            .await;
    }
    for address in ["C000", "C100"] {
        h.failure(
            "read_memory",
            json!({"address":address,"length":4}),
            "short or oversized memory response",
        )
        .await;
    }
    let result = h
        .success("read_memory", json!({"address":"C200","length":4}))
        .await;
    assert_eq!(result["data_hex"], "000180FF");
    assert_eq!(
        STANDARD
            .decode(result["data_base64"].as_str().unwrap())
            .unwrap(),
        [0, 1, 0x80, 0xff]
    );
    h.failure(
        "read_memory",
        json!({"address":"FFFF","length":2}),
        "may not pass $FFFF",
    )
    .await;
    assert_eq!(h.http.received_requests().await.unwrap().len(), 3);
    h.finish().await;
}

#[tokio::test]
async fn image_creation_encodes_paths_and_enforces_format_specific_tracks() {
    let h = Harness::new().await;
    Mock::given(method("PUT"))
        .and(path("/v1/files/Usb0/new%20disk%2Ed64:create_d64"))
        .and(query_param("tracks", "35"))
        .and(query_param("diskname", "MY DISK"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    Mock::given(method("PUT"))
        .and(path("/v1/files/Usb0/data%2Ednp:create_dnp"))
        .and(query_param("tracks", "255"))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    h.success(
        "create_disk_image",
        json!({"path":"/Usb0/new disk.d64","format":"d64","diskname":"MY DISK"}),
    )
    .await;
    h.success(
        "create_disk_image",
        json!({"path":"/Usb0/data.dnp","format":"dnp","tracks":255}),
    )
    .await;
    for (format, tracks) in [
        ("d64", 36),
        ("dnp", 0),
        ("dnp", 256),
        ("d71", 70),
        ("d81", 160),
    ] {
        let result = h
            .call(
                "create_disk_image",
                json!({"path":"/Usb0/test.img","format":format,"tracks":tracks}),
            )
            .await;
        assert_eq!(result.is_error, Some(true), "{format}/{tracks}");
    }
    h.failure(
        "create_disk_image",
        json!({"path":"/Usb0/data.dnp","format":"dnp"}),
        "requires tracks",
    )
    .await;
    assert_eq!(h.http.received_requests().await.unwrap().len(), 2);
    h.finish().await;
}

#[tokio::test]
async fn menu_and_text_snapshots_decode_their_distinct_binary_formats() {
    let h = Harness::new().await;
    let mut menu = vec![b' '; 2000];
    menu[..2].copy_from_slice(&[b'H', b'I' | 0x80]);
    menu[1000] = 0x25;
    Mock::given(method("GET"))
        .and(path("/v1/machine:menu_screen"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(menu)
                .insert_header("Content-Type", "application/octet-stream"),
        )
        .expect(1)
        .mount(&h.http)
        .await;
    let mut screen = vec![32; 1000];
    screen[..2].copy_from_slice(&[1, 0x82]);
    for (address, bytes) in [("0400", screen), ("D800", vec![0xf6; 1000])] {
        Mock::given(method("GET"))
            .and(path("/v1/machine:readmem"))
            .and(query_param("address", address))
            .and(query_param("length", "1000"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(bytes)
                    .insert_header("Content-Type", "application/octet-stream"),
            )
            .expect(1)
            .mount(&h.http)
            .await;
    }
    let menu = h.success("read_menu_screen", json!({})).await;
    assert!(menu["rows"][0].as_str().unwrap().starts_with("HI"));
    assert_eq!(menu["reverse"][1], true);
    assert_eq!(menu["foreground"][0], 5);
    assert_eq!(menu["background"][0], 2);
    let screen = h
        .success("read_text_screen", json!({"include_colors":true}))
        .await;
    assert_eq!(screen["address"], "0400");
    assert!(screen["rows"][0].as_str().unwrap().starts_with("AB"));
    assert_eq!(screen["reverse"][1], true);
    assert_eq!(screen["colors"][0], 6);
    h.finish().await;
}

#[tokio::test]
async fn input_batches_are_validated_before_any_device_events_are_sent() {
    let h = Harness::new().await;
    let events = json!([
        {"kind":"joystick","port":2,"inputs":["up","fire"],"transition":"press"},
        {"kind":"keyboard","inputs":["left_shift","a"],"transition":"tap"},
        {"kind":"release_all"}
    ]);
    Mock::given(method("POST"))
        .and(path("/v1/machine:input"))
        .and(body_json(json!({"events":events})))
        .respond_with(okay())
        .expect(1)
        .mount(&h.http)
        .await;
    h.success("send_input", json!({"events":events})).await;
    let invalid = [
        json!([]),
        json!([{"kind":"keyboard","inputs":["a","a"],"transition":"tap"}]),
        json!([{"kind":"keyboard","inputs":["restore"],"transition":"press"}]),
        json!([{"kind":"joystick","port":3,"inputs":["fire"],"transition":"tap"}]),
        json!(vec![json!({"kind":"release_all"}); 65]),
        json!(vec![
            json!({"kind":"keyboard","inputs":["left_shift","right_shift","cursor_left_right","cursor_up_down","commodore","run_stop","inst_del","clr_home"],"transition":"tap"});
            64
        ]),
    ];
    for events in invalid {
        assert_eq!(
            h.call("send_input", json!({"events":events}))
                .await
                .is_error,
            Some(true)
        );
    }
    assert_eq!(h.http.received_requests().await.unwrap().len(), 1);
    h.finish().await;
}

#[tokio::test]
async fn screenshot_restores_the_video_stream_this_server_started() {
    let h = Harness::with_capture(
        Some(std::net::Ipv4Addr::LOCALHOST),
        Duration::from_millis(300),
    )
    .await;
    for route in ["streams/video:start", "streams/video:stop"] {
        Mock::given(method("PUT"))
            .and(path(format!("/v1/{route}")))
            .respond_with(okay())
            .mount(&h.http)
            .await;
    }
    let video_calls = |requests: Vec<wiremock::Request>| -> Vec<String> {
        requests
            .iter()
            .map(|request| {
                let ip = request
                    .url
                    .query_pairs()
                    .find(|(name, _)| name == "ip")
                    .map(|(_, ip)| ip.into_owned());
                let route = request.url.path().rsplit('/').next().unwrap().to_owned();
                match ip {
                    Some(ip) if ip.starts_with("127.0.0.1:") => format!("{route} capture"),
                    Some(ip) => format!("{route} {ip}"),
                    None => route,
                }
            })
            .collect()
    };
    h.success(
        "start_stream",
        json!({"stream":"video","destination":"192.0.2.7:11000"}),
    )
    .await;
    // No packets arrive, so the capture fails; its cleanup still runs.
    h.failure("capture_screenshot", json!({}), "no video packets")
        .await;
    assert_eq!(
        video_calls(h.http.received_requests().await.unwrap()),
        [
            "video:start 192.0.2.7:11000",
            "video:start capture",
            "video:start 192.0.2.7:11000",
        ]
    );
    h.success("stop_stream", json!({"stream":"video"})).await;
    h.failure("capture_screenshot", json!({}), "no video packets")
        .await;
    assert_eq!(
        video_calls(h.http.received_requests().await.unwrap())[3..],
        ["video:stop", "video:start capture", "video:stop"]
    );
    h.finish().await;
}
