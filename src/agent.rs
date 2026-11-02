//! Agent processes. Devin runs the turns of one `devin acp` process one
//! after another, so the bridge gives each session its own process (like
//! Devin's terminal UI does) and keeps one more for `session/list`.

use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use serde_json::json;
use tokio::io::BufReader;
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

use crate::acp::{Acp, Incoming};
use crate::config::Config;

/// A connected agent. Its process (if any) is killed when dropped.
pub struct Agent {
    pub acp: Arc<Acp>,
    pub incoming: mpsc::UnboundedReceiver<Incoming>,
    pub child: Option<Child>,
}

/// Starts a new agent: `devin acp` in production, an in-memory fake in tests.
pub type Spawn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<Agent>> + Send>> + Send + Sync>;

pub fn devin(cfg: &Config) -> Spawn {
    let cfg = cfg.clone();
    Arc::new(move || {
        let cfg = cfg.clone();
        Box::pin(async move { start_devin(&cfg).await })
    })
}

/// Starts `devin acp` and runs `initialize`.
async fn start_devin(cfg: &Config) -> anyhow::Result<Agent> {
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
    let (acp, incoming) = Acp::start(BufReader::new(stdout), Box::new(stdin));
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
    tokio::time::timeout(Duration::from_secs(60), init)
        .await
        .context("the agent did not answer `initialize`")??;
    Ok(Agent {
        acp,
        incoming,
        child: Some(child),
    })
}
