//! Explicit opt-in hardware check through the real stdio MCP executable.
//! cargo run --example hardware_smoke -- --host IP [--screenshot] [--menu]
use anyhow::{Context, Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use clap::Parser;
use rmcp::{ServiceExt, model::CallToolRequestParams, transport::TokioChildProcess};
use serde_json::{Value, json};
use std::{path::PathBuf, time::Duration};
use tokio::process::Command;

#[derive(Parser)]
struct Options {
    #[arg(long)]
    host: String,
    #[arg(long, default_value_os_t = default_executable())]
    executable: PathBuf,
    #[arg(long, default_value = "target/hardware-smoke")]
    output: PathBuf,
    /// Temporarily starts and then stops video streaming.
    #[arg(long)]
    screenshot: bool,
    /// Read the current firmware menu without toggling it.
    #[arg(long)]
    menu: bool,
}
fn default_executable() -> PathBuf {
    PathBuf::from("target/release").join(format!("c64umcp{}", std::env::consts::EXE_SUFFIX))
}
#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    tokio::fs::create_dir_all(&options.output).await?;
    let mut command = Command::new(&options.executable);
    command.args([
        "--host",
        &options.host,
        "--timeout",
        "8",
        "--capture-timeout",
        "5",
    ]);
    command.kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let client = tokio::time::timeout(
        Duration::from_secs(15),
        ().serve(TokioChildProcess::new(command)?),
    )
    .await??;
    let mut report = json!({"host":options.host,"server":client.peer_info(),"results":{}});
    let tools = client.list_all_tools().await?;
    report["tool_count"] = json!(tools.len());
    let mut calls = vec![
        ("get_version", json!({})),
        ("get_info", json!({})),
        ("list_drives", json!({})),
        ("list_config_categories", json!({})),
        ("read_memory", json!({"address":"0400","length":16})),
        ("read_text_screen", json!({})),
        ("get_input_state", json!({})),
    ];
    if options.menu {
        calls.push(("read_menu_screen", json!({})));
    }
    let mut category = None;
    let mut failures = Vec::new();
    for (name, args) in calls {
        let result = client
            .call_tool(
                CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone()),
            )
            .await?;
        let wire = serde_json::to_value(&result)?;
        if name == "list_config_categories" {
            category = wire
                .pointer("/structuredContent/categories")
                .and_then(Value::as_array)
                .and_then(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .find(|name| matches!(*name, "Audio Mixer" | "Drive A Settings"))
                })
                .map(String::from);
        }
        println!(
            "{name}: {}",
            if result.is_error == Some(true) {
                "ERROR"
            } else {
                "OK"
            }
        );
        if result.is_error == Some(true)
            || matches!(
                name,
                "get_version" | "get_info" | "list_drives" | "get_input_state"
            )
        {
            println!("{}", serde_json::to_string_pretty(&wire)?);
        }
        if name == "read_text_screen"
            && let Some(text) = wire
                .pointer("/structuredContent/text")
                .and_then(Value::as_str)
        {
            println!("{text}");
        }
        if result.is_error == Some(true)
            && !matches!(name, "get_info" | "get_input_state" | "read_menu_screen")
        {
            failures.push(name);
        }
        report["results"][name] = wire;
        if name == "get_version" && result.is_error == Some(true) {
            break;
        }
    }
    if let Some(category) = category {
        let result = client
            .call_tool(
                CallToolRequestParams::new("get_config")
                    .with_arguments(json!({"category":category}).as_object().unwrap().clone()),
            )
            .await?;
        println!(
            "get_config ({category}): {}",
            if result.is_error == Some(true) {
                "ERROR"
            } else {
                "OK"
            }
        );
        if result.is_error == Some(true) {
            failures.push("get_config");
        }
        report["results"]["get_config"] = serde_json::to_value(result)?;
    }
    if options.screenshot && !failures.contains(&"get_version") {
        let result = client
            .call_tool(
                CallToolRequestParams::new("capture_screenshot").with_arguments(Default::default()),
            )
            .await?;
        let mut wire = serde_json::to_value(&result)?;
        for content in wire["content"].as_array_mut().context("missing content")? {
            if content["type"] == "image" {
                let bytes =
                    STANDARD.decode(content["data"].as_str().context("missing PNG data")?)?;
                ensure!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"), "invalid PNG");
                tokio::fs::write(options.output.join("screenshot.png"), &bytes).await?;
                content["data"] = json!("<saved to screenshot.png>");
                content["bytes"] = json!(bytes.len());
            }
        }
        println!(
            "capture_screenshot: {}",
            serde_json::to_string_pretty(&wire)?
        );
        if result.is_error == Some(true) {
            failures.push("capture_screenshot");
        }
        report["results"]["capture_screenshot"] = wire;
    }
    report["required_failures"] = json!(failures);
    tokio::fs::write(
        options.output.join("report.json"),
        serde_json::to_vec_pretty(&report)?,
    )
    .await?;
    client.cancel().await?;
    println!("Saved {}", options.output.join("report.json").display());
    ensure!(
        failures.is_empty(),
        "required hardware checks failed: {}",
        failures.join(", ")
    );
    Ok(())
}
