use anyhow::{Result, ensure};
use c64umcp::{device::Device, server::C64Server};
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use std::{net::Ipv4Addr, time::Duration};

#[derive(Parser)]
#[command(version, about)]
struct Options {
    /// C64U hostname, IP or HTTP base URL. No connection is made during startup.
    #[arg(long, env = "C64U_HOST")]
    host: Option<String>,
    /// HTTP request timeout in seconds.
    #[arg(long, env = "C64U_TIMEOUT", default_value_t = 30)]
    timeout: u64,
    /// This computer's IPv4 address reachable by the C64U for video capture.
    #[arg(long, env = "C64U_CAPTURE_IP")]
    capture_ip: Option<Ipv4Addr>,
    /// Local UDP port for video capture; 0 picks a free port for each capture.
    #[arg(long, env = "C64U_CAPTURE_PORT", default_value_t = 11000)]
    capture_port: u16,
    /// Maximum time to wait for a complete video frame, in seconds.
    #[arg(long, env = "C64U_CAPTURE_TIMEOUT", default_value_t = 5)]
    capture_timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let options = Options::parse();
    ensure!(
        (1..=300).contains(&options.timeout),
        "timeout must be 1..300 seconds"
    );
    ensure!(
        (1..=60).contains(&options.capture_timeout),
        "capture timeout must be 1..60 seconds"
    );
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();
    let password = std::env::var("C64U_PASSWORD").ok();
    let device = Device::new(
        options.host.as_deref(),
        password.as_deref(),
        Duration::from_secs(options.timeout),
    )?;
    let server = C64Server::new(
        device,
        options.capture_ip,
        options.capture_port,
        Duration::from_secs(options.capture_timeout),
    );
    let service = server.clone().serve(stdio()).await?;
    let outcome = service.waiting().await;
    // rmcp cancels remaining requests when the transport closes, but does not
    // join their handlers or the cleanup tasks they spawn when dropped.
    server.finish_shutdown().await;
    outcome?;
    Ok(())
}
