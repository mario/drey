//! drey: one language server per workspace, shared by every client.

// Dead code is a bug report about the design, not something to silence. If a
// field or function has no caller, either wire it up or delete it.
#![deny(dead_code, unused_imports, unused_mut, unused_variables)]

mod config;
mod daemon;
mod framing;
mod msg;
mod shim;
mod text;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "drey", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Act as a language server on stdio, backed by the shared daemon.
    /// This is what you put in your editor or agent config.
    Serve {
        /// Logical server name from the config, e.g. `rust-analyzer`.
        server: String,
        /// Arguments forwarded to the real server. Clients invoking it with
        /// different flags get different processes.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Run the daemon in the foreground. Normally autostarted by `serve`.
    Daemon,
    /// Show live backends, their clients and memory-relevant counts.
    Status {
        /// Emit JSON instead of a table.
        #[arg(long)]
        json: bool,
    },
    /// Release idle backends now instead of waiting for the sweep.
    Gc,
    /// Shut down every backend and the daemon.
    Stop,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // The shim owns stdio for the protocol, so logs must never go to stdout.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("DREY_LOG")
                .unwrap_or_else(|_| "drey=info".into()),
        )
        .init();

    match cli.command {
        Command::Serve { server, args } => shim::serve(server, args).await,
        Command::Daemon => daemon::run(true).await,
        Command::Gc => {
            let r = shim::control(daemon::Control::Gc).await?;
            println!("released {} backend(s)", r["released"]);
            Ok(())
        }
        Command::Stop => {
            shim::control(daemon::Control::Stop).await?;
            println!("drey stopped");
            Ok(())
        }
        Command::Status { json } => {
            let r = shim::control(daemon::Control::Status).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&r)?);
                return Ok(());
            }
            let rows = r.as_array().cloned().unwrap_or_default();
            if rows.is_empty() {
                println!("no backends running");
                return Ok(());
            }
            println!(
                "{:<16} {:>7} {:>8} {:>6} {:>6} {:>6} {:>8}  ROOTS",
                "SERVER", "PID", "CLIENTS", "DOCS", "DIRTY", "SWAPS", "UPTIME"
            );
            for b in rows {
                let s = |k: &str| b[k].as_str().unwrap_or("").to_string();
                let n = |k: &str| b[k].as_u64().unwrap_or(0);
                let roots: Vec<String> = b["roots"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|v| v.as_str().unwrap_or("").to_string())
                            .collect()
                    })
                    .unwrap_or_default();
                println!(
                    "{:<16} {:>7} {:>8} {:>6} {:>6} {:>6} {:>7}s  {}{}",
                    s("server"),
                    n("pid"),
                    n("clients"),
                    n("open_docs"),
                    n("dirty_docs"),
                    n("swaps"),
                    n("uptime_secs"),
                    roots.join(", "),
                    if b["private"].as_bool().unwrap_or(false) {
                        "  (forked)"
                    } else {
                        ""
                    },
                );
            }
            Ok(())
        }
    }
}
