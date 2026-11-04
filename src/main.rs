use anyhow::{Result, ensure};
use c64umcp::{device::Device, server::C64Server};
use clap::Parser;
use rmcp::{ServiceExt, transport::stdio};
use std::{net::Ipv4Addr, path::PathBuf, time::Duration};

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
    /// Directory for the per-device lock file that serializes operations with
    /// other server processes on this computer; default: the system temp directory.
    #[arg(long, env = "C64U_LOCK_DIR")]
    lock_dir: Option<PathBuf>,
    /// Maximum time to wait for another server process to finish with the
    /// device, in seconds.
    #[arg(long, env = "C64U_LOCK_TIMEOUT", default_value_t = 120)]
    lock_timeout: u64,
}

/// How long the service gets to stop after a signal cancels it, before the
/// server's own cleanup starts.
const SERVICE_STOP_TIMEOUT: Duration = Duration::from_millis(500);
/// Total shutdown bound for console close, logoff and system shutdown:
/// Windows kills the process about five seconds after those events.
const FORCED_SHUTDOWN_BUDGET: Duration = Duration::from_millis(3500);

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
    ensure!(
        (1..=600).contains(&options.lock_timeout),
        "lock timeout must be 1..600 seconds"
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
    )
    .with_lock_dir(options.lock_dir.unwrap_or_else(std::env::temp_dir))
    .with_lock_timeout(Duration::from_secs(options.lock_timeout));
    let mut signals = signals::Signals::new();
    let service = server.clone().serve(stdio()).await?;
    let token = service.cancellation_token();
    let waiting = service.waiting();
    tokio::pin!(waiting);
    let signal = tokio::select! {
        outcome = &mut waiting => {
            // rmcp cancels remaining requests when the transport closes, but
            // does not join their handlers or the cleanup tasks they spawn
            // when dropped.
            server.finish_shutdown().await;
            outcome?;
            return Ok(());
        }
        signal = signals.recv() => signal,
    };
    tracing::warn!("received {}; shutting down", signal.name());
    let budget = signal.forced().then_some(FORCED_SHUTDOWN_BUDGET);
    let cleanup = async {
        let started = tokio::time::Instant::now();
        // Cancel the service like a closed transport: in-flight handlers see
        // cancellation and hand their cleanup to bounded tasks.
        token.cancel();
        let _ = tokio::time::timeout(SERVICE_STOP_TIMEOUT, &mut waiting).await;
        server
            .finish_shutdown_within(budget.map(|budget| budget.saturating_sub(started.elapsed())))
            .await;
    };
    tokio::select! {
        () = cleanup => {}
        second = signals.recv() => {
            tracing::warn!("received {} during shutdown; exiting without cleanup", second.name());
        }
    }
    // Exit directly: the runtime's shutdown would wait for the blocking
    // stdin reader, which stays blocked while the client keeps stdin open.
    std::process::exit(signal.exit_code())
}

/// The shutdown signals this server handles.
mod signals {
    #[derive(Clone, Copy)]
    pub enum Signal {
        Interrupt,
        #[cfg(windows)]
        Break,
        #[cfg(windows)]
        Close,
        #[cfg(windows)]
        Logoff,
        #[cfg(windows)]
        Shutdown,
        #[cfg(unix)]
        Terminate,
        #[cfg(unix)]
        Hangup,
    }

    impl Signal {
        pub fn name(self) -> &'static str {
            match self {
                Self::Interrupt => "Ctrl-C",
                #[cfg(windows)]
                Self::Break => "Ctrl-Break",
                #[cfg(windows)]
                Self::Close => "a console close event",
                #[cfg(windows)]
                Self::Logoff => "a logoff event",
                #[cfg(windows)]
                Self::Shutdown => "a system shutdown event",
                #[cfg(unix)]
                Self::Terminate => "SIGTERM",
                #[cfg(unix)]
                Self::Hangup => "SIGHUP",
            }
        }

        /// Windows kills the process about five seconds after these.
        pub fn forced(self) -> bool {
            #[cfg(windows)]
            if matches!(self, Self::Close | Self::Logoff | Self::Shutdown) {
                return true;
            }
            false
        }

        /// Nonzero, following the 128 + signal number convention.
        pub fn exit_code(self) -> i32 {
            match self {
                #[cfg(unix)]
                Self::Terminate => 143,
                #[cfg(unix)]
                Self::Hangup => 129,
                _ => 130,
            }
        }
    }

    /// Wait on an optional listener; one that could not be registered never fires.
    macro_rules! next {
        ($listener:expr) => {
            async {
                match $listener.as_mut() {
                    Some(listener) => listener.recv().await,
                    None => std::future::pending().await,
                }
            }
        };
    }

    #[cfg(windows)]
    pub struct Signals {
        c: Option<tokio::signal::windows::CtrlC>,
        brk: Option<tokio::signal::windows::CtrlBreak>,
        close: Option<tokio::signal::windows::CtrlClose>,
        logoff: Option<tokio::signal::windows::CtrlLogoff>,
        shutdown: Option<tokio::signal::windows::CtrlShutdown>,
    }

    #[cfg(windows)]
    impl Signals {
        pub fn new() -> Self {
            use tokio::signal::windows::*;
            Self {
                c: ctrl_c().ok(),
                brk: ctrl_break().ok(),
                close: ctrl_close().ok(),
                logoff: ctrl_logoff().ok(),
                shutdown: ctrl_shutdown().ok(),
            }
        }

        pub async fn recv(&mut self) -> Signal {
            loop {
                tokio::select! {
                    Some(()) = next!(self.c) => return Signal::Interrupt,
                    Some(()) = next!(self.brk) => return Signal::Break,
                    Some(()) = next!(self.close) => return Signal::Close,
                    Some(()) = next!(self.logoff) => return Signal::Logoff,
                    Some(()) = next!(self.shutdown) => return Signal::Shutdown,
                    else => std::future::pending::<()>().await,
                }
            }
        }
    }

    #[cfg(unix)]
    pub struct Signals {
        interrupt: Option<tokio::signal::unix::Signal>,
        terminate: Option<tokio::signal::unix::Signal>,
        hangup: Option<tokio::signal::unix::Signal>,
    }

    #[cfg(unix)]
    impl Signals {
        pub fn new() -> Self {
            use tokio::signal::unix::{SignalKind, signal};
            Self {
                interrupt: signal(SignalKind::interrupt()).ok(),
                terminate: signal(SignalKind::terminate()).ok(),
                hangup: signal(SignalKind::hangup()).ok(),
            }
        }

        pub async fn recv(&mut self) -> Signal {
            loop {
                tokio::select! {
                    Some(()) = next!(self.interrupt) => return Signal::Interrupt,
                    Some(()) = next!(self.terminate) => return Signal::Terminate,
                    Some(()) = next!(self.hangup) => return Signal::Hangup,
                    else => std::future::pending::<()>().await,
                }
            }
        }
    }
}
