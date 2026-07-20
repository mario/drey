//! The client-facing half: a process that looks exactly like a language
//! server on stdio but forwards everything to the daemon.

use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{copy, stdin, stdout, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::config;
use crate::daemon::{Control, Hello, HELLO_METHOD};
use crate::framing::write_message;
use crate::msg;

/// Runs as a language server: connect to the daemon (starting it if needed),
/// announce ourselves, then splice stdio to the socket.
pub async fn serve(server: String, args: Vec<String>) -> Result<()> {
    let stream = connect_or_start().await?;
    let cwd = std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().into_owned());

    let (mut sock_rx, mut sock_tx) = stream.into_split();
    let hello = Hello {
        server,
        cwd,
        args,
        control: None,
    };
    write_message(
        &mut sock_tx,
        &msg::encode(&msg::notification(
            HELLO_METHOD,
            serde_json::to_value(hello)?,
        )),
    )
    .await?;

    // Splice both directions. Whichever side closes first ends the session,
    // which is what the client expects when a language server exits.
    let up = tokio::spawn(async move {
        let mut input = BufReader::new(stdin());
        let r = copy(&mut input, &mut sock_tx).await;
        let _ = sock_tx.shutdown().await;
        r
    });
    let down = tokio::spawn(async move {
        let mut output = stdout();
        let r = copy(&mut sock_rx, &mut output).await;
        let _ = output.flush().await;
        r
    });

    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}

/// Sends a one-shot control request and returns the daemon's reply.
pub async fn control(op: Control) -> Result<Value> {
    let stream = match op {
        // `status` and `gc` on a dead daemon should report emptiness, not
        // start one just to ask it a question.
        Control::Stop | Control::Status | Control::Gc => match connect().await {
            Some(s) => s,
            None => anyhow::bail!("no drey daemon is running"),
        },
    };
    let (rx, mut tx) = stream.into_split();
    let hello = Hello {
        server: String::new(),
        cwd: None,
        args: Vec::new(),
        control: Some(op),
    };
    write_message(
        &mut tx,
        &msg::encode(&msg::notification(
            HELLO_METHOD,
            serde_json::to_value(hello)?,
        )),
    )
    .await?;

    let mut reader = BufReader::new(rx);
    let raw = crate::framing::read_message(&mut reader)
        .await?
        .context("daemon closed without replying")?;
    Ok(serde_json::from_slice(&raw)?)
}

async fn connect() -> Option<UnixStream> {
    UnixStream::connect(config::socket_path()).await.ok()
}

/// Connects, autostarting the daemon if nothing is listening.
async fn connect_or_start() -> Result<UnixStream> {
    if let Some(s) = connect().await {
        return Ok(s);
    }

    spawn_daemon()?;

    // Several shims may race to start the daemon; all but one will lose the
    // bind and exit, so keep retrying until someone is listening.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut backoff = Duration::from_millis(20);
    while std::time::Instant::now() < deadline {
        tokio::time::sleep(backoff).await;
        if let Some(s) = connect().await {
            return Ok(s);
        }
        backoff = (backoff * 2).min(Duration::from_millis(400));
    }
    anyhow::bail!(
        "daemon did not come up within 10s; see {}",
        config::log_path().display()
    )
}

fn spawn_daemon() -> Result<()> {
    let exe = std::env::current_exe().context("locating the drey binary")?;
    let dir = config::runtime_dir();
    std::fs::create_dir_all(&dir)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(config::log_path())?;

    std::process::Command::new(exe)
        .arg("daemon")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(log.try_clone()?))
        .stderr(std::process::Stdio::from(log))
        .spawn()
        .context("starting the drey daemon")?;
    Ok(())
}
