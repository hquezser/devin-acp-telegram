//! Agent Client Protocol client: JSON-RPC 2.0, one message per line over
//! the agent's stdio. Generic over the transport so tests can plug an
//! in-process fake agent.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicI64, Ordering};

use anyhow::anyhow;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tracing::warn;

type Reply = Result<Value, String>;
type Writer = Box<dyn AsyncWrite + Send + Unpin>;

/// Something the agent sent us on its own.
#[derive(Debug)]
pub enum Incoming {
    /// `session/update` notification params.
    Update(Value),
    /// Agent → client request (permission prompt…), to be answered.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
}

pub struct Acp {
    writer: tokio::sync::Mutex<Writer>,
    next: AtomicI64,
    pending: Mutex<HashMap<i64, oneshot::Sender<Reply>>>,
}

impl Acp {
    /// Starts the reader task; incoming notifications/requests go to the
    /// returned channel, which closes when the agent's output ends.
    pub fn start<R>(
        reader: R,
        writer: Writer,
    ) -> (std::sync::Arc<Acp>, mpsc::UnboundedReceiver<Incoming>)
    where
        R: AsyncBufRead + Send + Unpin + 'static,
    {
        let acp = std::sync::Arc::new(Acp {
            writer: tokio::sync::Mutex::new(writer),
            next: AtomicI64::new(1),
            pending: Mutex::default(),
        });
        let (tx, rx) = mpsc::unbounded_channel();
        let me = acp.clone();
        tokio::spawn(async move {
            let mut lines = reader.lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                match (v["method"].as_str(), v.get("id")) {
                    (None, Some(_)) => me.resolve(&v),
                    (Some(m), Some(id)) => {
                        tx.send(Incoming::Request {
                            id: id.clone(),
                            method: m.to_string(),
                            params: v["params"].clone(),
                        })
                        .ok();
                    }
                    (Some("session/update"), None) => {
                        tx.send(Incoming::Update(v["params"].clone())).ok();
                    }
                    _ => {}
                }
            }
            // Agent gone: fail every pending request.
            me.pending.lock().unwrap_or_else(|e| e.into_inner()).clear();
        });
        (acp, rx)
    }

    async fn write(&self, v: &Value) -> anyhow::Result<()> {
        let mut line = v.to_string();
        line.push('\n');
        let mut w = self.writer.lock().await;
        w.write_all(line.as_bytes()).await?;
        w.flush().await?;
        Ok(())
    }

    pub async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, tx);
        self.write(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        rx.await
            .map_err(|_| anyhow!("agent stopped"))?
            .map_err(|e| anyhow!("{method}: {e}"))
    }

    pub async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.write(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    pub async fn respond(&self, id: Value, result: Value) {
        let v = json!({"jsonrpc": "2.0", "id": id, "result": result});
        if let Err(e) = self.write(&v).await {
            warn!(error = %e, "acp: response not written");
        }
    }

    pub async fn respond_error(&self, id: Value, code: i64, message: &str) {
        let v = json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}});
        if let Err(e) = self.write(&v).await {
            warn!(error = %e, "acp: error not written");
        }
    }

    fn resolve(&self, v: &Value) {
        let Some(id) = v["id"].as_i64() else { return };
        let tx = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&id);
        let Some(tx) = tx else { return };
        let reply = match v.get("error") {
            Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
            None => Ok(v["result"].clone()),
        };
        tx.send(reply).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn requests_responses_and_incoming() {
        let (client_io, agent_io) = tokio::io::duplex(1 << 16);
        let (cr, cw) = tokio::io::split(client_io);
        let (ar, mut aw) = tokio::io::split(agent_io);
        let (acp, mut rx) = Acp::start(BufReader::new(cr), Box::new(cw));
        // Fake agent: answers request 1, errors request 2, then pushes a
        // notification and a request of its own.
        tokio::spawn(async move {
            let mut lines = BufReader::new(ar).lines();
            for _ in 0..2 {
                let v: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                let out = if v["method"] == "ok" {
                    json!({"jsonrpc": "2.0", "id": v["id"], "result": {"x": 1}})
                } else {
                    json!({"jsonrpc": "2.0", "id": v["id"], "error": {"code": 1, "message": "nope"}})
                };
                aw.write_all(format!("{out}\n").as_bytes()).await.unwrap();
            }
            aw.write_all(b"not json\n").await.unwrap();
            aw.write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"session/update\",\"params\":{\"sessionId\":\"s\"}}\n")
                .await
                .unwrap();
            aw.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":\"q\",\"method\":\"session/request_permission\",\"params\":{}}\n")
                .await
                .unwrap();
        });
        assert_eq!(acp.request("ok", json!({})).await.unwrap()["x"], 1);
        let e = acp.request("ko", json!({})).await.unwrap_err();
        assert_eq!(e.to_string(), "ko: nope");
        match rx.recv().await.unwrap() {
            Incoming::Update(p) => assert_eq!(p["sessionId"], "s"),
            other => panic!("{other:?}"),
        }
        match rx.recv().await.unwrap() {
            Incoming::Request { id, method, .. } => {
                assert_eq!(
                    (id, method.as_str()),
                    (json!("q"), "session/request_permission")
                );
            }
            other => panic!("{other:?}"),
        }
    }
}
