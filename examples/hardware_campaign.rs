//! Explicit, destructive scratch-session campaign. Never part of automatic tests.
#[path = "support/fixtures.rs"]
mod fixtures;
use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::{Parser, ValueEnum};
use rmcp::{
    RoleClient, ServiceExt,
    model::{CallToolRequestParams, ClientRequest, Request},
    service::RunningService,
    transport::TokioChildProcess,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    net::{Ipv4Addr, UdpSocket},
    path::PathBuf,
    time::{Duration, Instant},
};
use tokio::process::Command;

type Client = RunningService<RoleClient, ()>;
#[derive(Clone, Copy, ValueEnum, PartialEq)]
enum Stage {
    All,
    Programs,
    Cartridge,
    Media,
    Drives,
    Soak,
}
#[derive(Parser)]
struct Options {
    #[arg(long)]
    host: String,
    /// Required acknowledgement: replaces the running program and changes volatile state.
    #[arg(long)]
    scratch: bool,
    #[arg(long, default_value_os_t=default_executable())]
    executable: PathBuf,
    #[arg(long, default_value = "target/hardware-campaign")]
    output: PathBuf,
    #[arg(long, value_enum, default_value = "all")]
    stage: Stage,
    #[arg(long, default_value_t = 60)]
    soak_cycles: usize,
}
fn default_executable() -> PathBuf {
    PathBuf::from("target/release").join(format!("c64umcp{}", std::env::consts::EXE_SUFFIX))
}
#[derive(Clone, Copy)]
enum Expected {
    Success,
    Optional,
    Rejected,
}
struct Campaign {
    client: Client,
    options: Options,
    records: Vec<Value>,
    catalog: BTreeMap<String, Value>,
    drive_settings: Option<Value>,
    streams_settings: Option<Value>,
    memory_backup: Option<Vec<u8>>,
    menu_opened: bool,
    mutations_started: bool,
    drive_mutated: bool,
    screenshot_number: usize,
}
async fn connect(options: &Options, bad_capture: bool) -> Result<Client> {
    let mut command = Command::new(&options.executable);
    command.args([
        "--host",
        &options.host,
        "--timeout",
        "8",
        "--capture-timeout",
        if bad_capture { "1" } else { "5" },
    ]);
    if bad_capture {
        command.args(["--capture-ip", "127.0.0.1"]);
    }
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    Ok(tokio::time::timeout(
        Duration::from_secs(15),
        ().serve(TokioChildProcess::new(command)?),
    )
    .await??)
}
fn params(name: &str, args: Value) -> CallToolRequestParams {
    CallToolRequestParams::new(name.to_owned()).with_arguments(args.as_object().unwrap().clone())
}
fn bytes(value: &Value) -> Result<Vec<u8>> {
    Ok(STANDARD.decode(
        value["data_base64"]
            .as_str()
            .context("missing memory bytes")?,
    )?)
}
fn source(name: &str, data: &[u8]) -> Value {
    json!({"kind":"base64","filename":name,"data":STANDARD.encode(data)})
}

impl Campaign {
    async fn save(&self) -> Result<()> {
        let report = json!({"host":self.options.host,"server":self.client.peer_info(),"tool_matrix":self.catalog,"checks":self.records});
        tokio::fs::write(
            self.options.output.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )
        .await?;
        Ok(())
    }
    async fn invoke(
        &mut self,
        label: &str,
        name: &str,
        args: Value,
        expected: Expected,
    ) -> Result<Option<Value>> {
        let start = Instant::now();
        let response = tokio::time::timeout(
            Duration::from_secs(30),
            self.client.call_tool(params(name, args)),
        )
        .await;
        let mut wire = match response {
            Ok(Ok(r)) => serde_json::to_value(r)?,
            other => {
                self.records.push(json!({"label":label,"tool":name,"status":"failed","error":format!("{other:?}")}));
                self.catalog.insert(
                    name.into(),
                    json!({"status":"failed","reason":"MCP request failed"}),
                );
                self.save().await?;
                bail!("{label}: MCP request failed");
            }
        };
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;
        let error = wire["isError"] == true;
        let text = wire["content"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let unsupported = error
            && matches!(expected, Expected::Optional)
            && (text.contains("HTTP 404") || text.contains("HTTP 501"));
        let passed = match expected {
            Expected::Rejected => error,
            _ => !error,
        };
        let status = if unsupported {
            "unsupported"
        } else if passed {
            "passed"
        } else {
            "failed"
        };
        if let Some(contents) = wire["content"].as_array_mut() {
            for content in contents {
                if content["type"] == "image" {
                    let png =
                        STANDARD.decode(content["data"].as_str().context("missing image")?)?;
                    let mut reader = png::Decoder::new(std::io::Cursor::new(&png)).read_info()?;
                    let mut image = vec![0; reader.output_buffer_size().context("PNG size")?];
                    let frame = reader.next_frame(&mut image)?;
                    ensure!(
                        frame.width == 384 && matches!(frame.height, 240 | 272),
                        "unexpected capture dimensions"
                    );
                    self.screenshot_number += 1;
                    let filename = format!(
                        "{:03}-{}.png",
                        self.screenshot_number,
                        label.replace(' ', "-")
                    );
                    tokio::fs::write(self.options.output.join(&filename), &png).await?;
                    content["data"] = json!({"saved":filename,"bytes":png.len(),"width":frame.width,"height":frame.height});
                }
            }
        }
        println!("{status:11} {label} ({elapsed:.1} ms)");
        if error {
            println!("  {text}");
        }
        self.records.push(json!({"label":label,"tool":name,"status":status,"expected_rejection":matches!(expected,Expected::Rejected),"elapsed_ms":elapsed,"response":wire}));
        if !matches!(expected, Expected::Rejected) {
            let prior = self.catalog.get(name).and_then(|v| v["status"].as_str());
            if prior != Some("failed") {
                self.catalog
                    .insert(name.into(), json!({"status":status,"last_check":label}));
            }
        }
        self.save().await?;
        ensure!(passed || unsupported, "{label}: {text}");
        Ok(if unsupported {
            None
        } else {
            Some(wire.get("structuredContent").cloned().unwrap_or(wire))
        })
    }
    async fn call(&mut self, label: &str, name: &str, args: Value) -> Result<Value> {
        self.invoke(label, name, args, Expected::Success)
            .await?
            .context("missing success")
    }
    async fn assertion(&mut self, label: &str, passed: bool) -> Result<()> {
        println!(
            "{} {label}",
            if passed { "verified   " } else { "FAILED     " }
        );
        self.records.push(
            json!({"label":label,"status":if passed{"passed"}else{"failed"},"kind":"assertion"}),
        );
        self.save().await?;
        ensure!(passed, "assertion failed: {label}");
        Ok(())
    }
    async fn ram(&mut self, label: &str, address: &str, length: usize) -> Result<Vec<u8>> {
        bytes(
            &self
                .call(
                    label,
                    "read_memory",
                    json!({"address":address,"length":length}),
                )
                .await?,
        )
    }
    async fn baseline(&mut self) -> Result<()> {
        let tools = self.client.list_all_tools().await?;
        for tool in tools {
            self.catalog.insert(
                tool.name.to_string(),
                json!({"status":"skipped","reason":"not yet exercised"}),
            );
        }
        for name in [
            "poweroff_machine",
            "save_config_to_flash",
            "load_config_from_flash",
            "reset_config_defaults",
            "create_disk_image",
            "load_drive_rom",
            "write_debug_register",
        ] {
            self.catalog.insert(
                name.into(),
                json!({"status":"skipped","reason":"excluded from hardware campaign"}),
            );
        }
        self.call("API version", "get_version", json!({})).await?;
        self.invoke("device identity", "get_info", json!({}), Expected::Optional)
            .await?;
        self.call("drive baseline", "list_drives", json!({}))
            .await?;
        self.call(
            "configuration categories",
            "list_config_categories",
            json!({}),
        )
        .await?;
        let streams = self
            .call(
                "stream configuration snapshot",
                "get_config",
                json!({"category":"Data Streams"}),
            )
            .await?;
        self.streams_settings = streams.get("Data Streams").cloned();
        ensure!(
            self.streams_settings.is_some(),
            "missing stream settings snapshot"
        );
        if matches!(self.options.stage, Stage::All | Stage::Drives) {
            let config = self
                .call(
                    "drive B settings snapshot",
                    "get_config",
                    json!({"category":"Drive B Settings"}),
                )
                .await?;
            self.drive_settings = config.get("Drive B Settings").cloned();
            ensure!(
                self.drive_settings.is_some(),
                "missing drive B settings snapshot"
            );
        }
        // Refuse to begin mutations unless every captured value can be sent
        // back through set_config_batch during cleanup.
        for (category, snapshot) in [
            ("Data Streams", self.streams_settings.as_ref()),
            ("Drive B Settings", self.drive_settings.as_ref()),
        ] {
            if let Some(snapshot) = snapshot {
                let items = snapshot.as_object().with_context(|| {
                    format!("{category} snapshot must be a configuration object")
                })?;
                ensure!(!items.is_empty(), "{category} snapshot must not be empty");
                for (item, value) in items {
                    ensure!(
                        !item.is_empty(),
                        "{category} snapshot contains an empty item name"
                    );
                    ensure!(
                        value.is_string() || value.is_i64() || value.is_u64(),
                        "{category}/{item} snapshot value cannot be restored; expected a string or integer"
                    );
                }
            }
        }
        tokio::fs::write(
            self.options.output.join("snapshots.json"),
            serde_json::to_vec_pretty(
                &json!({"streams":self.streams_settings,"drive_b":self.drive_settings}),
            )?,
        )
        .await?;
        self.ram("baseline RAM", "0400", 16).await?;
        self.invoke(
            "input capability",
            "get_input_state",
            json!({}),
            Expected::Optional,
        )
        .await?;
        self.invoke(
            "debug register read",
            "read_debug_register",
            json!({}),
            Expected::Optional,
        )
        .await?;
        self.mutations_started = true;
        Ok(())
    }
    async fn start_heartbeat(&mut self) -> Result<()> {
        self.call(
            "run heartbeat PRG",
            "run_prg",
            json!({"source":source("heartbeat.prg",&fixtures::heartbeat_prg())}),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(250)).await;
        let screen = self
            .call("heartbeat screen", "read_text_screen", json!({}))
            .await?;
        self.assertion(
            "heartbeat program displayed marker",
            screen["text"]
                .as_str()
                .unwrap_or("")
                .contains("C64U MCP TEST"),
        )
        .await?;
        let first = self.ram("heartbeat before", "C000", 4).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let next = self.ram("heartbeat after", "C000", 4).await?;
        self.assertion("heartbeat advances", first != next).await
    }
    async fn programs(&mut self) -> Result<()> {
        let prg = fixtures::heartbeat_prg();
        let file = self.options.output.join("heartbeat.prg");
        tokio::fs::write(&file, &prg).await?;
        let absolute = tokio::fs::canonicalize(&file).await?;
        self.call(
            "load PRG local without running",
            "load_prg",
            json!({"source":{"kind":"local","path":absolute.to_string_lossy()}}),
        )
        .await?;
        let loaded = self
            .ram("loaded PRG readback", "0801", prg.len() - 2)
            .await?;
        self.assertion("load-only PRG bytes exact", loaded == prg[2..])
            .await?;
        let first = self.ram("load-only counter before", "C000", 4).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = self.ram("load-only counter after", "C000", 4).await?;
        self.assertion("load-only did not start heartbeat", first == after)
            .await?;
        self.start_heartbeat().await?;
        self.call("pause heartbeat", "pause_machine", json!({}))
            .await?;
        let first = self.ram("paused counter before", "C000", 4).await?;
        tokio::time::sleep(Duration::from_millis(150)).await;
        let after = self.ram("paused counter after", "C000", 4).await?;
        self.assertion("pause freezes CPU heartbeat", first == after)
            .await?;
        self.call("resume heartbeat", "resume_machine", json!({}))
            .await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let after = self.ram("resumed counter", "C000", 4).await?;
        self.assertion("resume restarts heartbeat", first != after)
            .await?;
        self.memory_backup = Some(self.ram("scratch RAM backup", "C100", 256).await?);
        for length in [1usize, 128, 129, 256] {
            let pattern = (0..length)
                .map(|i| (i as u8).wrapping_mul(73).wrapping_add(length as u8))
                .collect::<Vec<_>>();
            self.call(
                &format!("write {length} bytes"),
                "write_memory",
                json!({"address":"C100","data_hex":hex(&pattern)}),
            )
            .await?;
            let actual = self
                .ram(&format!("readback {length} bytes"), "C100", length)
                .await?;
            self.assertion(&format!("{length}-byte RAM roundtrip"), actual == pattern)
                .await?;
        }
        self.ram("last address read", "FFFF", 1).await?;
        for (tool, args) in [
            ("write_memory", json!({"address":"FFFF","data_hex":"0102"})),
            ("read_memory", json!({"address":"FFFF","length":2})),
            ("write_memory", json!({"address":"C100","data_hex":"GG"})),
            (
                "send_input",
                json!({"events":[{"kind":"keyboard","inputs":["restore","a"],"transition":"tap"}]}),
            ),
        ] {
            self.invoke("reject invalid arguments", tool, args, Expected::Rejected)
                .await?;
        }
        self.restore_memory().await?;
        self.call("heartbeat screenshot", "capture_screenshot", json!({}))
            .await?;
        self.call("reset to BASIC", "reset_machine", json!({}))
            .await?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        self.basic_screen("reset BASIC screen").await?;
        self.call("reboot machine", "reboot_machine", json!({}))
            .await?;
        tokio::time::sleep(Duration::from_millis(700)).await;
        self.basic_screen("reboot BASIC screen").await?;
        self.call("open firmware menu", "toggle_menu", json!({}))
            .await?;
        self.menu_opened = true;
        self.call("menu screenshot", "capture_screenshot", json!({}))
            .await?;
        self.invoke(
            "menu screen while open",
            "read_menu_screen",
            json!({}),
            Expected::Optional,
        )
        .await?;
        self.call("close firmware menu", "toggle_menu", json!({}))
            .await?;
        self.menu_opened = false;
        if self
            .catalog
            .get("get_input_state")
            .is_some_and(|v| v["status"] == "passed")
        {
            self.call(
                "type BASIC arithmetic",
                "type_text",
                json!({"text":"PRINT 2+2\n"}),
            )
            .await?;
            self.call(
                "raw RETURN tap",
                "send_input",
                json!({"events":[{"kind":"keyboard","inputs":["return"],"transition":"tap"}]}),
            )
            .await?;
            self.call("release inputs", "release_all_inputs", json!({}))
                .await?;
        } else {
            for name in ["type_text", "send_input", "release_all_inputs"] {
                self.catalog.insert(name.into(),json!({"status":"unsupported","reason":"input endpoint GET returned 404/501; no RAM fallback"}));
            }
        }
        self.cartridge().await?;
        self.media().await?;
        Ok(())
    }
    async fn cartridge(&mut self) -> Result<()> {
        self.call(
            "clear cartridge phase marker",
            "write_memory",
            json!({"address":"C010","data_hex":"00"}),
        )
        .await?;
        tokio::fs::write(
            self.options.output.join("original.crt"),
            fixtures::cartridge(),
        )
        .await?;
        self.call(
            "run original CRT",
            "run_crt",
            json!({"source":source("original.crt",&fixtures::cartridge())}),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Err(error) = self.wait_screen("cartridge screen", "C64U MCP CRT").await {
            let _ = self.ram("cartridge startup phase", "C010", 1).await;
            let _ = self.ram("cartridge ROM header", "8000", 256).await;
            return Err(error);
        }
        self.call("reboot after temporary CRT", "reboot_machine", json!({}))
            .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        self.basic_screen("BASIC after cartridge").await?;
        Ok(())
    }
    async fn media(&mut self) -> Result<()> {
        for (tool, name, data) in [
            ("play_sid", "original.sid", fixtures::sid()),
            ("play_mod", "original.mod", fixtures::module()),
        ] {
            let accepted = self
                .invoke(
                    &format!("start silent {name}"),
                    tool,
                    json!({"source":source(name,&data)}),
                    Expected::Optional,
                )
                .await?;
            if accepted.is_some() && tool == "play_sid" {
                let deadline = Instant::now() + Duration::from_secs(10);
                let first = loop {
                    let current = self
                        .ram("SID init signature and counter", "1020", 2)
                        .await?;
                    if current[0] == 0x53 || Instant::now() >= deadline {
                        break current;
                    }
                    tokio::time::sleep(Duration::from_millis(250)).await;
                };
                self.assertion("SID init executed", first[0] == 0x53)
                    .await?;
                tokio::time::sleep(Duration::from_millis(200)).await;
                let next = self.ram("SID play counter", "1020", 2).await?;
                self.assertion("SID play routine executes", first[1] != next[1])
                    .await?;
            }
            self.call(
                &format!("screen after {name}"),
                "capture_screenshot",
                json!({}),
            )
            .await?;
            self.call(&format!("reset after {name}"), "reset_machine", json!({}))
                .await?;
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        self.stream_checks().await?;
        self.capture_recovery().await?;
        Ok(())
    }
    async fn basic_screen(&mut self, label: &str) -> Result<()> {
        self.wait_screen(label, "READY.").await
    }
    async fn wait_screen(&mut self, label: &str, marker: &str) -> Result<()> {
        // Reset/reboot acknowledgments precede KERNAL initialization on hardware.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let screen = self.call(label, "read_text_screen", json!({})).await?;
            if screen["text"].as_str().unwrap_or("").contains(marker) {
                // Text RAM can retain READY during the black phase of reboot.
                let vic = self.ram("VIC display readiness", "D011", 1).await?;
                if vic[0] & 0x10 != 0 {
                    return self.assertion(&format!("{marker} visible"), true).await;
                }
            }
            if Instant::now() >= deadline {
                let _ = self
                    .call("failed screen assertion", "capture_screenshot", json!({}))
                    .await;
                return self
                    .assertion(&format!("{marker} visible within 10 seconds"), false)
                    .await;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    async fn restore_memory(&mut self) -> Result<()> {
        if let Some(data) = self.memory_backup.clone() {
            self.call(
                "restore scratch RAM",
                "write_memory",
                json!({"address":"C100","data_hex":hex(&data)}),
            )
            .await?;
            let restored = self.ram("restored RAM readback", "C100", 256).await?;
            self.assertion("scratch RAM restored", restored == data)
                .await?;
            self.memory_backup = None;
        }
        Ok(())
    }
    async fn drives(&mut self) -> Result<()> {
        let status = self
            .call("drive B initial state", "list_drives", json!({}))
            .await?;
        let b = drive_b(&status).context("drive B missing")?;
        ensure!(
            b["image_file"].as_str().unwrap_or("").is_empty(),
            "drive B already has media; refusing to replace it without preserving its unknown mount mode"
        );
        let original = self.drive_settings.clone().context("missing snapshot")?;
        self.drive_mutated = true;
        self.call("enable drive B", "enable_drive", json!({"drive":"b"}))
            .await?;
        self.call(
            "drive B 1581 mode",
            "set_drive_mode",
            json!({"drive":"b","mode":"1581"}),
        )
        .await?;
        let status = self.call("1581 readback", "list_drives", json!({})).await?;
        self.assertion(
            "drive mode switched",
            drive_b(&status).is_some_and(|b| b["type"] == "1581"),
        )
        .await?;
        self.call(
            "drive B 1541 mode",
            "set_drive_mode",
            json!({"drive":"b","mode":"1541"}),
        )
        .await?;
        let meta = self
            .call(
                "drive bus metadata",
                "get_config",
                json!({"category":"Drive B Settings","item":"Drive Bus ID"}),
            )
            .await?;
        let bus = &meta["Drive B Settings"]["Drive Bus ID"];
        let current = bus["current"].as_i64().context("bus id current")?;
        let min = bus["min"].as_i64().unwrap_or(8);
        let max = bus["max"].as_i64().unwrap_or(11);
        let replacement = (min..=max)
            .find(|n| {
                *n != current
                    && !status["drives"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_object)
                        .flat_map(|d| d.values())
                        .any(|d| d["enabled"] == true && d["bus_id"].as_i64() == Some(*n))
            })
            .context("no unoccupied advertised scratch bus ID")?;
        self.call(
            "temporary bus ID change",
            "set_config",
            json!({"category":"Drive B Settings","item":"Drive Bus ID","value":replacement}),
        )
        .await?;
        let read = self
            .call(
                "bus ID readback",
                "get_config",
                json!({"category":"Drive B Settings","item":"Drive Bus ID"}),
            )
            .await?;
        self.assertion(
            "config change readback",
            read["Drive B Settings"]["Drive Bus ID"]["current"] == replacement,
        )
        .await?;
        self.call(
            "batch restore bus ID",
            "set_config_batch",
            json!({"settings":{"Drive B Settings":{"Drive Bus ID":current}}}),
        )
        .await?;
        self.call("mount uploaded blank readonly disk","mount_disk",json!({"drive":"b","source":source("scratch.d64",&fixtures::disk_d64()),"image_type":"d64","mode":"readonly"})).await?;
        let status = self
            .call("mounted scratch disk state", "list_drives", json!({}))
            .await?;
        let b = drive_b(&status).context("drive B missing")?;
        let filename = b["image_file"].as_str().unwrap_or("");
        let dir = b["image_path"].as_str().unwrap_or("");
        self.assertion("scratch disk mounted", !filename.is_empty())
            .await?;
        let path = if filename.starts_with('/') {
            filename.to_owned()
        } else {
            format!("{}/{filename}", dir.trim_end_matches('/'))
        };
        if path.starts_with('/') && path.len() > 1 {
            self.invoke(
                "scratch file metadata",
                "file_info",
                json!({"path":path}),
                Expected::Optional,
            )
            .await?;
            self.call("remount scratch disk by device path","mount_disk",json!({"drive":"b","source":{"kind":"device","path":path},"image_type":"d64","mode":"unlinked"})).await?;
        }
        self.call("unlink scratch disk", "unlink_disk", json!({"drive":"b"}))
            .await?;
        self.call("reset drive B", "reset_drive", json!({"drive":"b"}))
            .await?;
        self.call("eject scratch disk", "eject_disk", json!({"drive":"b"}))
            .await?;
        self.call("disable drive B", "disable_drive", json!({"drive":"b"}))
            .await?;
        self.call(
            "restore drive B settings",
            "set_config_batch",
            json!({"settings":{"Drive B Settings":original}}),
        )
        .await?;
        let read = self
            .call(
                "restored drive settings",
                "get_config",
                json!({"category":"Drive B Settings"}),
            )
            .await?;
        self.assertion(
            "drive settings restored exactly",
            read["Drive B Settings"] == original,
        )
        .await?;
        Ok(())
    }
    async fn stream_checks(&mut self) -> Result<()> {
        let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        let base = if self.options.host.contains("://") {
            self.options.host.clone()
        } else {
            format!("http://{}", self.options.host)
        };
        let url = url::Url::parse(&base)?;
        socket.connect((
            url.host_str().context("missing host")?,
            url.port_or_known_default().unwrap_or(80),
        ))?;
        let destination = socket.local_addr()?.to_string();
        for stream in ["video", "audio", "debug"] {
            let result = self
                .invoke(
                    &format!("direct {stream} start"),
                    "start_stream",
                    json!({"stream":stream,"destination":destination}),
                    Expected::Optional,
                )
                .await;
            // Always stop even if the start response was lost.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let stopped = self
                .invoke(
                    &format!("direct {stream} stop"),
                    "stop_stream",
                    json!({"stream":stream}),
                    Expected::Optional,
                )
                .await;
            result?;
            stopped?;
        }
        Ok(())
    }
    async fn capture_recovery(&mut self) -> Result<()> {
        let bad = connect(&self.options, true).await?;
        let response = bad
            .call_tool(params("capture_screenshot", json!({})))
            .await?;
        self.assertion(
            "misconfigured capture reports timeout",
            response.is_error == Some(true),
        )
        .await?;
        let handle = bad
            .send_cancellable_request(
                ClientRequest::CallToolRequest(Request::new(params(
                    "capture_screenshot",
                    json!({}),
                ))),
                Default::default(),
            )
            .await?;
        tokio::time::sleep(Duration::from_millis(200)).await;
        handle
            .cancel(Some("hardware cancellation check".into()))
            .await?;
        let recovery = tokio::time::timeout(
            Duration::from_secs(5),
            bad.call_tool(params("get_version", json!({}))),
        )
        .await??;
        self.assertion(
            "cancelled capture releases gate",
            recovery.is_error != Some(true),
        )
        .await?;
        bad.cancel().await?;
        self.call(
            "capture after timeout and cancellation",
            "capture_screenshot",
            json!({}),
        )
        .await?;
        Ok(())
    }
    async fn soak(&mut self) -> Result<()> {
        self.start_heartbeat().await?;
        let start = Instant::now();
        let mut latencies = Vec::new();
        for cycle in 0..self.options.soak_cycles {
            let cycle_start = tokio::time::Instant::now();
            let started = Instant::now();
            let (a, b, c) = tokio::join!(
                self.client.call_tool(params("get_version", json!({}))),
                self.client
                    .call_tool(params("read_memory", json!({"address":"C000","length":4}))),
                self.client.call_tool(params("list_drives", json!({})))
            );
            for response in [a, b, c] {
                ensure!(
                    response?.is_error != Some(true),
                    "soak concurrent call failed"
                );
            }
            self.call(
                &format!("soak {:02}", cycle + 1),
                "capture_screenshot",
                json!({}),
            )
            .await?;
            latencies.push(started.elapsed().as_secs_f64() * 1000.0);
            if cycle + 1 < self.options.soak_cycles {
                tokio::time::sleep_until(cycle_start + Duration::from_secs(5)).await;
            }
        }
        latencies.sort_by(f64::total_cmp);
        if !latencies.is_empty() {
            self.records.push(json!({"label":"soak summary","status":"passed","cycles":latencies.len(),"elapsed_seconds":start.elapsed().as_secs_f64(),"cycle_median_ms":latencies[latencies.len()/2],"cycle_p95_ms":latencies[((latencies.len()*95).div_ceil(100)-1).min(latencies.len()-1)]}));
            self.save().await?;
        }
        Ok(())
    }
    async fn run(&mut self) -> Result<()> {
        self.baseline().await?;
        match self.options.stage {
            Stage::All => {
                self.programs().await?;
                self.drives().await?;
                self.soak().await?;
            }
            Stage::Programs => self.programs().await?,
            Stage::Cartridge => self.cartridge().await?,
            Stage::Media => self.media().await?,
            Stage::Drives => self.drives().await?,
            Stage::Soak => self.soak().await?,
        }
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<()> {
        if !self.mutations_started {
            return Ok(());
        }
        let mut failures = Vec::new();
        if self.menu_opened {
            if let Err(e) = self
                .call("cleanup close menu", "toggle_menu", json!({}))
                .await
            {
                failures.push(e.to_string());
            }
            self.menu_opened = false;
        }
        for stream in ["video", "audio", "debug"] {
            if let Err(e) = self
                .invoke(
                    &format!("cleanup stop {stream}"),
                    "stop_stream",
                    json!({"stream":stream}),
                    Expected::Optional,
                )
                .await
            {
                failures.push(e.to_string());
            }
        }
        if let Err(e) = self.restore_memory().await {
            failures.push(e.to_string());
        }
        if let Some(settings) = self.drive_settings.clone()
            && self.drive_mutated
        {
            // This campaign only mutates an initially empty drive B.
            if let Err(e) = self
                .call("cleanup eject B", "eject_disk", json!({"drive":"b"}))
                .await
            {
                failures.push(e.to_string());
            }
            if let Err(e) = self
                .call(
                    "cleanup restore drive B",
                    "set_config_batch",
                    json!({"settings":{"Drive B Settings":settings}}),
                )
                .await
            {
                failures.push(e.to_string());
            }
            let verified = async {
                let read = self
                    .call(
                        "cleanup drive B settings readback",
                        "get_config",
                        json!({"category":"Drive B Settings"}),
                    )
                    .await?;
                self.assertion(
                    "cleanup drive B settings restored exactly",
                    read["Drive B Settings"] == settings,
                )
                .await?;
                let runtime = self
                    .call("cleanup drive B runtime readback", "list_drives", json!({}))
                    .await?;
                let current =
                    drive_b(&runtime).context("drive B missing after cleanup restoration")?;
                let original = self
                    .records
                    .iter()
                    .find(|record| record["label"] == "drive baseline")
                    .and_then(|record| drive_b(&record["response"]["structuredContent"]))
                    .context("missing drive B runtime baseline for restoration verification")?;
                let restored = ["enabled", "bus_id", "type", "rom"].iter().all(|field| {
                    current.get(*field).is_some() && current[*field] == original[*field]
                }) && current["image_file"].as_str() == Some("");
                self.assertion("cleanup drive B runtime restored", restored)
                    .await
            }
            .await;
            if let Err(e) = verified {
                failures.push(e.to_string());
            }
        }
        if self
            .catalog
            .get("get_input_state")
            .is_some_and(|v| v["status"] == "passed")
            && let Err(e) = self
                .call("cleanup release inputs", "release_all_inputs", json!({}))
                .await
        {
            failures.push(e.to_string());
        }
        for tool in ["resume_machine", "reboot_machine"] {
            if let Err(e) = self.call(&format!("cleanup {tool}"), tool, json!({})).await {
                failures.push(e.to_string());
            }
        }
        tokio::time::sleep(Duration::from_millis(750)).await;
        if let Err(e) = self.basic_screen("final BASIC READY").await {
            failures.push(e.to_string());
        }
        if let Err(e) = self
            .call("final BASIC screenshot", "capture_screenshot", json!({}))
            .await
        {
            failures.push(e.to_string());
        }
        // The final capture starts video after the earlier stop loop. Attempt
        // a final stop even if that capture's own cleanup reported a failure.
        if let Err(e) = self
            .invoke(
                "cleanup final video stop",
                "stop_stream",
                json!({"stream":"video"}),
                Expected::Optional,
            )
            .await
        {
            failures.push(e.to_string());
        }
        if let Some(settings) = self.streams_settings.clone() {
            if let Err(e) = self
                .call(
                    "restore stream configuration",
                    "set_config_batch",
                    json!({"settings":{"Data Streams":settings}}),
                )
                .await
            {
                failures.push(e.to_string());
            }
            let verified = async {
                let read = self
                    .call(
                        "restored stream settings",
                        "get_config",
                        json!({"category":"Data Streams"}),
                    )
                    .await?;
                self.assertion(
                    "stream configuration restored exactly",
                    read["Data Streams"] == settings,
                )
                .await
            }
            .await;
            if let Err(e) = verified {
                failures.push(e.to_string());
            }
        }
        self.records.push(json!({"label":"cleanup summary","status":if failures.is_empty() {"passed"}else{"failed"},"errors":failures}));
        self.save().await?;
        ensure!(
            failures.is_empty(),
            "cleanup failures: {}",
            failures.join("; ")
        );
        Ok(())
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}
fn drive_b(status: &Value) -> Option<&Value> {
    status["drives"].as_array()?.iter().find_map(|d| d.get("b"))
}
#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    ensure!(
        options.scratch,
        "--scratch is required; this campaign replaces the running program"
    );
    ensure!(options.soak_cycles <= 180, "soak cycles must be at most180");
    ensure!(
        !options.output.join("report.json").exists(),
        "output already contains a campaign report; choose a fresh --output directory to preserve prior evidence"
    );
    tokio::fs::create_dir_all(&options.output).await?;
    let client = connect(&options, false).await?;
    let mut campaign = Campaign {
        client,
        options,
        records: vec![],
        catalog: BTreeMap::new(),
        drive_settings: None,
        streams_settings: None,
        memory_backup: None,
        menu_opened: false,
        mutations_started: false,
        drive_mutated: false,
        screenshot_number: 0,
    };
    let outcome = tokio::select! {
        r=campaign.run()=>r,
        _=tokio::signal::ctrl_c()=>Err(anyhow::anyhow!("campaign interrupted")),
    };
    let cleanup = campaign.cleanup().await;
    if let Err(error) = &outcome {
        campaign.records.push(
            json!({"label":"campaign failure","status":"failed","error":format!("{error:#}")}),
        );
    }
    campaign.save().await?;
    campaign.client.cancel().await?;
    outcome?;
    cleanup?;
    println!(
        "Campaign complete; report: {}",
        campaign.options.output.join("report.json").display()
    );
    Ok(())
}
