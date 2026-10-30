//! devin-acp-telegram — drive Devin CLI from Telegram.
//!
//! Runs `devin acp` (Agent Client Protocol over stdio) and a dedicated
//! Telegram bot on the same machine: no server, no open port.

mod acp;
mod bridge;
mod config;
#[cfg(test)]
mod e2e;
mod i18n;
mod state;
mod telegram;

use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, anyhow};
use serde_json::json;
use tokio::io::BufReader;
use tokio::process::Command;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
devin-acp-telegram — drive Devin CLI from Telegram

Usage: devin-acp-telegram [DIR] [-- PROMPT…]

  DIR      default working directory for new sessions (default: current)
  PROMPT   optional first instruction (starts a session right away)

Configuration: environment variables or ~/.config/devin-acp-telegram/config.env
  TELEGRAM_BOT_TOKEN       bot token from @BotFather (required)
  TELEGRAM_ALLOWED_USERS   comma-separated Telegram user ids (required)
  DEVIN_PROJECTS           aliases: app=~/code/app,api=~/code/api
  DEVIN_MODE               mode of new sessions (accept-edits, smart, ask, plan, bypass)
  DEVIN_MODEL              model of new sessions (e.g. opus)
  DEVIN_CAFFEINE=0         don't keep the Mac awake
  DEVIN_TG_LANG=fr         French messages
  DEVIN_BIN, DEVIN_ACP_ARGS, DEVIN_TG_STATE_DIR, DEVIN_TG_CONFIG

Full documentation: README.md";

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

async fn hostname() -> String {
    Command::new("hostname")
        .arg("-s")
        .output()
        .await
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "host".to_string())
}

/// One life of the bridge: returns Ok on a stop signal, Err when the
/// agent dies (the caller restarts it).
async fn run(cfg: &config::Config, initial: Option<String>) -> anyhow::Result<()> {
    let mut child = Command::new(&cfg.devin_bin)
        .arg("acp")
        .args(&cfg.acp_args)
        .current_dir(&cfg.cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "cannot start `{} acp` (is Devin CLI installed?)",
                cfg.devin_bin
            )
        })?;
    let stdin = child.stdin.take().context("agent stdin")?;
    let stdout = child.stdout.take().context("agent stdout")?;
    let (acp, incoming) = acp::Acp::start(BufReader::new(stdout), Box::new(stdin));
    let init = acp.request(
        "initialize",
        json!({
            "protocolVersion": 1,
            // No fs/terminal on our side: the agent keeps its own tools.
            "clientCapabilities": {
                "fs": {"readTextFile": false, "writeTextFile": false},
                "terminal": false,
            },
            "clientInfo": {"name": "devin-acp-telegram", "version": env!("CARGO_PKG_VERSION")},
        }),
    );
    let init = tokio::time::timeout(Duration::from_secs(60), init)
        .await
        .context("the agent did not answer `initialize`")??;
    info!(
        agent = init["agentInfo"]["title"].as_str().unwrap_or("?"),
        "connected to the agent"
    );
    let (bridge, mut tasks) = bridge::Bridge::start(cfg.clone(), acp, incoming, hostname().await);
    bridge.startup(&mut tasks, initial).await;
    let r = tokio::select! {
        s = child.wait() => Err(anyhow!("agent exited ({})", s.map(|s| s.to_string()).unwrap_or_default())),
        _ = shutdown_signal() => Ok(()),
    };
    tasks.abort_all();
    bridge
        .shutdown(r.as_ref().err().map(|e| e.to_string()).as_deref())
        .await;
    r
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return Ok(());
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("devin-acp-telegram {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let (dir, prompt) = match args.iter().position(|a| a == "--") {
        Some(i) => (args[..i].first().cloned(), Some(args[i + 1..].join(" "))),
        None => (args.first().cloned(), None),
    };
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,reqwest=warn,hyper=warn")),
        )
        .init();
    let cfg = config::Config::load(dir.as_deref())?;
    std::fs::create_dir_all(&cfg.state_dir)
        .with_context(|| format!("cannot create {}", cfg.state_dir.display()))?;
    // One bridge per bot: two would both answer every message.
    let lock = std::fs::File::create(cfg.state_dir.join("lock"))?;
    if lock.try_lock().is_err() {
        anyhow::bail!(
            "another devin-acp-telegram is already running with this state directory ({})",
            cfg.state_dir.display()
        );
    }
    info!(cwd = cfg.cwd, "starting");
    let mut initial = prompt;
    loop {
        match run(&cfg, initial.take()).await {
            Ok(()) => return Ok(()),
            Err(e) => {
                error!(error = %e, "bridge stopped, restarting in 5 s");
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    _ = shutdown_signal() => return Ok(()),
                }
            }
        }
    }
}
