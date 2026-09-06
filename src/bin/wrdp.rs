//! `wrdp` server binary entrypoint.
//!
//! Wires CLI/config/runtime diagnostics and launches the production listener
//! flow that binds authenticated connections to managed per-user sessions.

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use wrdp::config::Config;

/// Command-line arguments for wrdp
#[derive(Parser, Debug)]
#[command(name = "wrdp")]
#[command(version, about = "Wayland Remote Desktop Server", long_about = None)]
pub struct Args {
    /// Configuration file path
    #[arg(short, long)]
    pub config: Option<String>,

    /// Listen address
    #[arg(short, long, env = "WRDP_LISTEN_ADDR")]
    pub listen: Option<String>,

    /// Listen port
    #[arg(short, long, env = "WRDP_PORT")]
    pub port: Option<u16>,

    /// Verbose logging (can be specified multiple times)
    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Log format (json|pretty|compact)
    #[arg(long, default_value = "pretty")]
    pub log_format: String,

    /// Show detected compositor and portal capabilities and exit
    ///
    /// Useful for debugging detection issues and understanding what
    /// runtime capture/input capabilities are currently available.
    #[arg(long)]
    pub show_capabilities: bool,

    /// Output format for --show-capabilities (text|json)
    ///
    /// Default is human-readable text. Use json for machine parsing,
    /// especially for integration with the GUI.
    #[arg(long, default_value = "text")]
    pub format: String,

    /// Run diagnostics and exit
    ///
    /// Tests deployment detection, portal connection, credential storage,
    /// and other components. Helpful for troubleshooting setup issues.
    #[arg(long)]
    pub diagnose: bool,

    /// Print a default wrdp.ini to stdout and exit
    ///
    /// Generates a fully commented configuration file with all default
    /// values. Redirect to a file to use as a starting point:
    ///   wrdp --generate-config > wrdp.ini
    #[arg(long)]
    pub generate_config: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Generate default config and exit (no logging, no config loading)
    if args.generate_config {
        match Config::generate_default_ini() {
            Ok(ini) => {
                print!("{ini}");
                return Ok(());
            }
            Err(e) => {
                eprintln!("Error generating config: {e}");
                std::process::exit(1);
            }
        }
    }

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(|| "/etc/wrdp/wrdp.ini".to_string());

    // Configuration errors are fatal. Starting with defaults after a typo can
    // silently change listener, authentication or clipboard policy.
    let config = Config::load(&config_path)
        .with_context(|| format!("failed to load configuration {config_path}"))?;

    // Machine-readable output modes: skip logging banner to keep stdout clean
    let quiet_mode = args.show_capabilities && args.format == "json";

    // Initialize logging (uses config.logging, CLI args override)
    init_logging(&args, &config.logging, quiet_mode)?;

    if !quiet_mode {
        info!("════════════════════════════════════════════════════════");
        info!("  wrdp v{}", env!("CARGO_PKG_VERSION"));
        info!(
            "  Commit: {}",
            option_env!("GIT_HASH").unwrap_or("vendored")
        );
        info!(
            "  Profile: {}",
            if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
        );
        info!("════════════════════════════════════════════════════════");
    }

    if args.show_capabilities {
        return wrdp::runtime::print_capabilities(args.format == "json").await;
    }

    if args.diagnose {
        return wrdp::runtime::print_diagnostics().await;
    }

    wrdp::runtime::log_startup_diagnostics();

    // Apply CLI overrides to the loaded config.
    let config = config.with_overrides(args.listen.clone(), args.port);

    info!("Configuration loaded successfully");
    tracing::debug!("Runtime configuration initialized");

    if wrdp::rdp::server::systemd_socket_activation_without_pending_connection() {
        info!(
            "Systemd socket is active but no RDP connection is pending; exiting before compositor session initialization"
        );
        return Ok(());
    }

    Box::pin(wrdp::rdp::server::production::run(config)).await
}

fn init_logging(
    args: &Args,
    logging_config: &wrdp::config::types::LoggingConfig,
    quiet_mode: bool,
) -> Result<()> {
    let level = if quiet_mode {
        "error"
    } else {
        match args.verbose {
            0 => match logging_config.level.as_str() {
                "trace" | "debug" | "info" | "warn" | "error" => &logging_config.level,
                _ => "info",
            },
            1 => "debug",
            _ => "trace",
        }
    };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(format!(
            "wrdp={level},ironrdp_server={level},ironrdp_egfx={level},ironrdp_dvc={level},ironrdp=info,warn"
        ))
    });
    let registry = tracing_subscriber::registry().with(filter);
    match args.log_format.as_str() {
        "json" => registry
            .with(tracing_subscriber::fmt::layer().json())
            .init(),
        "compact" => registry
            .with(tracing_subscriber::fmt::layer().compact())
            .init(),
        _ => registry
            .with(tracing_subscriber::fmt::layer().pretty())
            .init(),
    }
    Ok(())
}
