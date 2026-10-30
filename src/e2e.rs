//! End-to-end test: the whole bridge against a fake Telegram Bot API
//! (local HTTP server) and a fake ACP agent (in-memory pipe).

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Path;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::acp::Acp;
use crate::bridge::Bridge;

type Queue = Arc<Mutex<VecDeque<Value>>>;

/// Fake Bot API: `getUpdates` serves `queue`, every other call is
/// reported on the channel and answered with an incrementing message id
/// (and topic 77).
async fn fake_telegram() -> (String, Queue, mpsc::UnboundedReceiver<(String, Value)>) {
    let queue: Queue = Arc::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let next = Arc::new(Mutex::new(100i64));
    let q = queue.clone();
    let app = Router::new().route(
        "/{bot}/{method}",
        post(
            move |Path((_, method)): Path<(String, String)>, Json(body): Json<Value>| {
                let (q, tx, next) = (q.clone(), tx.clone(), next.clone());
                async move {
                    if method == "getUpdates" {
                        for _ in 0..20 {
                            let batch: Vec<Value> = q.lock().unwrap().drain(..).collect();
                            if !batch.is_empty() {
                                return Json(json!({"ok": true, "result": batch}));
                            }
                            tokio::time::sleep(Duration::from_millis(50)).await;
                        }
                        return Json(json!({"ok": true, "result": []}));
                    }
                    let id = {
                        let mut n = next.lock().unwrap();
                        *n += 1;
                        *n
                    };
                    tx.send((method, body)).ok();
                    Json(json!({"ok": true, "result": {"message_id": id, "message_thread_id": 77}}))
                }
            },
        ),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://127.0.0.1:{port}"), queue, rx)
}

async fn send(w: &mut (impl tokio::io::AsyncWrite + Unpin), v: Value) {
    w.write_all(format!("{v}\n").as_bytes()).await.unwrap();
}

/// Fake Devin: each prompt runs a tool that needs permission, then
/// answers "Done: 200" (or stops on cancel / rejection).
async fn fake_agent(
    reader: impl tokio::io::AsyncRead + Unpin,
    mut writer: impl tokio::io::AsyncWrite + Unpin,
) {
    let mut lines = BufReader::new(reader).lines();
    let mut sessions = 0;
    let mut prompt: Option<(Value, String)> = None;
    while let Ok(Some(line)) = lines.next_line().await {
        let v: Value = serde_json::from_str(&line).unwrap();
        let id = v["id"].clone();
        let sid = v["params"]["sessionId"].as_str().unwrap_or("").to_string();
        let update = |sid: &str, u: Value| {
            json!({"jsonrpc": "2.0", "method": "session/update",
                   "params": {"sessionId": sid, "update": u}})
        };
        match v["method"].as_str() {
            Some("session/new") => {
                sessions += 1;
                let r = json!({"sessionId": format!("s{sessions}"),
                               "modes": {"currentModeId": "accept-edits"}});
                send(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": id, "result": r}),
                )
                .await;
            }
            Some("session/prompt") => {
                send(
                    &mut writer,
                    update(
                        &sid,
                        json!({"sessionUpdate": "tool_call", "toolCallId": "t1",
                                        "title": "Ran curl", "kind": "execute"}),
                    ),
                )
                .await;
                send(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": "perm", "method": "session/request_permission",
                           "params": {"sessionId": sid,
                                      "toolCall": {"toolCallId": "t1",
                                                   "_meta": {"cognition.ai/editableCommand": "curl -sI example.com"}},
                                      "options": [
                                          {"optionId": "allow_once", "name": "Allow", "kind": "allow_once"},
                                          {"optionId": "reject_once", "name": "Reject", "kind": "reject_once"}]}}),
                )
                .await;
                prompt = Some((id, sid));
            }
            Some("session/cancel") => {
                if let Some((pid, _)) = prompt.take() {
                    let r = json!({"stopReason": "cancelled"});
                    send(
                        &mut writer,
                        json!({"jsonrpc": "2.0", "id": pid, "result": r}),
                    )
                    .await;
                }
            }
            Some("session/set_mode") => {
                let mode = v["params"]["modeId"].clone();
                send(
                    &mut writer,
                    update(
                        &sid,
                        json!({"sessionUpdate": "current_mode_update", "currentModeId": mode}),
                    ),
                )
                .await;
                send(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                )
                .await;
            }
            Some(_) => {
                send(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                )
                .await;
            }
            // Our permission request answered by the bridge.
            None => {
                let Some((pid, psid)) = prompt.take() else {
                    continue;
                };
                let allowed = v["result"]["outcome"]["optionId"] == "allow_once";
                let cancelled = v["result"]["outcome"]["outcome"] == "cancelled";
                if allowed {
                    for chunk in ["Done: ", "200"] {
                        send(
                            &mut writer,
                            update(
                                &psid,
                                json!({"sessionUpdate": "agent_message_chunk",
                                                 "content": {"type": "text", "text": chunk}}),
                            ),
                        )
                        .await;
                    }
                }
                let reason = if cancelled { "cancelled" } else { "end_turn" };
                let r = json!({"stopReason": reason});
                send(
                    &mut writer,
                    json!({"jsonrpc": "2.0", "id": pid, "result": r}),
                )
                .await;
            }
        }
    }
}

struct Harness {
    queue: Queue,
    calls: mpsc::UnboundedReceiver<(String, Value)>,
    next_update: i64,
}

impl Harness {
    fn push(&mut self, kind: &str, body: Value) {
        self.next_update += 1;
        self.queue
            .lock()
            .unwrap()
            .push_back(json!({"update_id": self.next_update, kind: body}));
    }

    fn text(&mut self, chat: i64, from: i64, text: &str) {
        let kind = if chat == from {
            "private"
        } else {
            "supergroup"
        };
        self.push(
            "message",
            json!({"message_id": self.next_update, "from": {"id": from},
                   "chat": {"id": chat, "type": kind, "is_forum": chat != from}, "text": text}),
        );
    }

    /// Waits for a call matching `method` and `pred` (others are skipped).
    async fn expect(&mut self, method: &str, pred: impl Fn(&Value) -> bool) -> Value {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut skipped = Vec::new();
        loop {
            let left = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(left, self.calls.recv()).await {
                Ok(Some((m, b))) if m == method && pred(&b) => return b,
                Ok(Some((m, b))) => skipped.push(format!("{m} {}", b["text"])),
                _ => panic!("no {method} call matching; seen: {skipped:#?}"),
            }
        }
    }
}

fn text_is(b: &Value, s: &str) -> bool {
    b["text"].as_str().is_some_and(|t| t.contains(s))
}

#[tokio::test]
async fn remote_control_round_trip() {
    let (api, queue, calls) = fake_telegram().await;
    let dir = std::env::temp_dir().join(format!("dat-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cfg = crate::config::test_config(&api, dir.clone());

    let (client, agent) = tokio::io::duplex(1 << 16);
    let (cr, cw) = tokio::io::split(client);
    let (ar, aw) = tokio::io::split(agent);
    tokio::spawn(fake_agent(ar, aw));
    let (acp, incoming) = Acp::start(BufReader::new(cr), Box::new(cw));
    let (bridge, mut tasks) = Bridge::start(cfg, acp, incoming, "mac".into());
    bridge.startup(&mut tasks, None).await;
    let mut h = Harness {
        queue,
        calls,
        next_update: 0,
    };
    h.expect("sendMessage", |b| {
        text_is(b, "Devin bridge online") && b["chat_id"] == 42
    })
    .await;

    // Private message with a project alias: new session in /srv/app.
    h.text(42, 42, "app: check example.com");
    let b = h.expect("sendMessage", |b| text_is(b, "🟢 Working")).await;
    assert!(text_is(&b, "🤖 [app] check example.com"), "{b}");
    assert_eq!(b["disable_notification"], true);
    // Permission prompt with the agent's own options.
    let b = h
        .expect("sendMessage", |b| text_is(b, "🔐 Allow? Ran curl"))
        .await;
    // The command is a code block (entity), not literal backticks.
    assert!(text_is(&b, "\ncurl -sI example.com"), "{b}");
    assert_eq!(b["entities"][0]["type"], "pre");
    let data = b["reply_markup"]["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        b["reply_markup"]["inline_keyboard"][1][0]["text"],
        "⛔ Reject"
    );

    // A stranger's tap is ignored; the owner's is applied once.
    let tap = |from: i64| {
        json!({"id": "cb", "from": {"id": from}, "data": data,
               "message": {"message_id": 999, "chat": {"id": 42, "type": "private"}}})
    };
    h.push("callback_query", tap(7));
    h.push("callback_query", tap(42));
    h.expect("editMessageText", |b| text_is(b, "→ ✅ Allow"))
        .await;
    let b = h.expect("sendMessage", |b| text_is(b, "Done: 200")).await;
    assert_eq!(b["disable_notification"], false);
    h.expect("editMessageText", |b| text_is(b, "⚪ Your turn"))
        .await;
    h.push("callback_query", tap(42));
    h.expect("answerCallbackQuery", |b| text_is(b, "Already answered"))
        .await;

    // Follow-up without prefix → same session (👀); /mode, /sessions.
    h.text(42, 42, "and the headers?");
    h.expect("setMessageReaction", |b| b["chat_id"] == 42).await;
    h.expect("sendMessage", |b| text_is(b, "🔐 Allow?")).await;
    h.text(42, 42, "/stop");
    h.expect("sendMessage", |b| text_is(b, "⏹ Stop requested"))
        .await;
    h.expect("sendMessage", |b| text_is(b, "⏹ Interrupted."))
        .await;
    h.text(42, 42, "/mode plan");
    h.expect("sendMessage", |b| text_is(b, "⚙️ Mode: plan"))
        .await;
    h.text(42, 42, "/sessions");
    let b = h
        .expect("sendMessage", |b| text_is(b, "open session(s)"))
        .await;
    assert!(text_is(&b, "[app] check example.com (s1)"), "{b}");

    // Strangers are ignored; an unknown group only accepts /link.
    h.text(42, 7, "rm -rf /");
    h.text(-1001234567, 42, "hello");
    h.text(-1001234567, 42, "/link");
    h.expect("sendMessage", |b| {
        text_is(b, "✅ Group linked") && b["chat_id"] == -1001234567
    })
    .await;
    // In General: a new session, in its own topic.
    h.text(-1001234567, 42, "write the changelog");
    let b = h
        .expect("createForumTopic", |b| b["chat_id"] == -1001234567)
        .await;
    assert_eq!(b["name"], "🤖 [default] write the changelog");
    // The General topic gets a link to the new topic…
    let b = h
        .expect("sendMessage", |b| text_is(b, "https://t.me/c/1234567/77"))
        .await;
    assert_eq!(b["chat_id"], -1001234567);
    assert_eq!(b["message_thread_id"], Value::Null);
    // …and the session talks inside its topic, without a header.
    let b = h
        .expect("sendMessage", |b| {
            b["message_thread_id"] == 77 && b["disable_notification"] == true
        })
        .await;
    assert!(!text_is(&b, "🤖"), "no header inside a topic: {b}");

    // State survives: sessions, linked group, Telegram offset.
    tasks.abort_all();
    let st = crate::state::State::load(&dir);
    assert_eq!(st.group, Some(-1001234567));
    assert_eq!(st.sessions.len(), 2);
    assert_eq!(st.sessions["s2"].thread, Some(77));
    assert!(st.offset > 0);
    std::fs::remove_dir_all(&dir).ok();
}
