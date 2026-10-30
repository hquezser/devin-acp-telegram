//! Minimal Telegram Bot API client. The token is part of every URL path:
//! errors are stripped of URLs (`without_url`) so it never reaches a log.

use std::time::Duration;

use anyhow::{anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::warn;

const TIMEOUT: Duration = Duration::from_secs(15);
/// `getUpdates` long-polling wait.
pub const POLL_TIMEOUT: u64 = 30;
/// Max Telegram message length (4096) with some margin.
pub const CHUNK: usize = 3900;

pub struct Bot {
    http: reqwest::Client,
    base: String,
}

#[derive(Deserialize)]
struct Envelope {
    ok: bool,
    #[serde(default)]
    result: Value,
    description: Option<String>,
    #[serde(default)]
    parameters: Value,
}

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    pub message: Option<Message>,
    pub callback_query: Option<Callback>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    #[serde(default)]
    pub message_id: i64,
    pub chat: Chat,
    pub from: Option<User>,
    pub text: Option<String>,
    pub message_thread_id: Option<i64>,
    #[serde(default)]
    pub is_topic_message: bool,
    pub reply_to_message: Option<Box<Message>>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub is_forum: bool,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct Callback {
    pub id: String,
    pub from: User,
    pub data: Option<String>,
    pub message: Option<Message>,
}

/// Where a message goes: a chat, optionally a forum topic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dest {
    pub chat: i64,
    pub thread: Option<i64>,
}

/// Splits long text into ≤ `CHUNK`-char pieces, preferably at newlines.
pub fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text.trim();
    while rest.chars().count() > CHUNK {
        let cut = rest
            .char_indices()
            .nth(CHUNK)
            .map_or(rest.len(), |(i, _)| i);
        let at = rest[..cut]
            .rfind('\n')
            .filter(|&i| i > cut / 2)
            .unwrap_or(cut);
        out.push(rest[..at].trim_end().to_string());
        rest = rest[at..].trim_start();
    }
    if !rest.is_empty() {
        out.push(rest.to_string());
    }
    out
}

/// Link to a forum topic (`-100123…` supergroup ids).
pub fn topic_link(chat: i64, thread: i64) -> String {
    let id = chat.to_string();
    let id = id
        .strip_prefix("-100")
        .unwrap_or(id.trim_start_matches('-'));
    format!("https://t.me/c/{id}/{thread}")
}

impl Bot {
    pub fn new(api_url: &str, token: &str) -> Bot {
        let http = reqwest::Client::builder()
            .user_agent(concat!("devin-acp-telegram/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client");
        Bot {
            http,
            base: format!("{api_url}/bot{token}"),
        }
    }

    /// Bot API call; retries on 429 (`retry_after`). Returns `result`.
    pub async fn call(&self, method: &str, body: &Value) -> anyhow::Result<Value> {
        self.call_with(method, body, TIMEOUT).await
    }

    async fn call_with(
        &self,
        method: &str,
        body: &Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        for _ in 0..4 {
            let resp = self
                .http
                .post(format!("{}/{method}", self.base))
                .timeout(timeout)
                .json(body)
                .send()
                .await
                .map_err(|e| anyhow!("Telegram unreachable: {}", e.without_url()))?;
            let status = resp.status();
            let env: Envelope = resp
                .json()
                .await
                .map_err(|e| anyhow!("Telegram: unreadable response ({})", e.without_url()))?;
            if env.ok {
                return Ok(env.result);
            }
            if status.as_u16() == 429 {
                let wait = env.parameters["retry_after"].as_u64().unwrap_or(3).min(60);
                warn!(method, wait, "telegram: rate limited");
                tokio::time::sleep(Duration::from_secs(wait)).await;
                continue;
            }
            bail!(
                "Telegram {method}: {}",
                env.description.unwrap_or_else(|| format!("HTTP {status}"))
            );
        }
        bail!("Telegram {method}: still rate limited")
    }

    pub async fn updates(&self, offset: i64) -> anyhow::Result<Vec<Update>> {
        let r = self
            .call_with(
                "getUpdates",
                &json!({
                    "offset": offset,
                    "timeout": POLL_TIMEOUT,
                    "allowed_updates": ["message", "callback_query"],
                }),
                Duration::from_secs(POLL_TIMEOUT) + TIMEOUT,
            )
            .await?;
        Ok(serde_json::from_value(r)?)
    }

    /// Sends one message; returns its id.
    pub async fn send(
        &self,
        to: Dest,
        text: &str,
        silent: bool,
        markup: Option<Value>,
    ) -> anyhow::Result<i64> {
        let mut body = json!({
            "chat_id": to.chat,
            "text": text,
            "disable_notification": silent,
            "link_preview_options": {"is_disabled": true},
        });
        if let Some(t) = to.thread {
            body["message_thread_id"] = json!(t);
        }
        if let Some(m) = markup {
            body["reply_markup"] = m;
        }
        let r = self.call("sendMessage", &body).await?;
        Ok(r["message_id"].as_i64().unwrap_or_default())
    }

    /// Sends a possibly long text in chunks; returns the message ids.
    pub async fn send_long(&self, to: Dest, text: &str, silent: bool) -> anyhow::Result<Vec<i64>> {
        let mut ids = Vec::new();
        for part in chunks(text) {
            ids.push(self.send(to, &part, silent, None).await?);
        }
        Ok(ids)
    }

    /// Edits a message; "not modified" counts as success.
    pub async fn edit(&self, chat: i64, msg: i64, text: &str) -> anyhow::Result<()> {
        match self
            .call(
                "editMessageText",
                &json!({"chat_id": chat, "message_id": msg, "text": text,
                        "link_preview_options": {"is_disabled": true}}),
            )
            .await
        {
            Err(e) if e.to_string().contains("not modified") => Ok(()),
            r => r.map(|_| ()),
        }
    }

    /// Best effort: logs instead of failing.
    pub async fn try_call(&self, method: &str, body: Value) {
        if let Err(e) = self.call(method, &body).await {
            warn!(error = %e, method, "telegram: call failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_long_text_on_newlines() {
        assert_eq!(chunks("  short  "), ["short"]);
        assert!(chunks("").is_empty());
        let line = "x".repeat(1000);
        let text = [line.as_str(); 6].join("\n");
        let parts = chunks(&text);
        assert_eq!(parts.len(), 2);
        assert!(parts.iter().all(|p| p.chars().count() <= CHUNK));
        assert_eq!(parts[0].lines().count(), 3);
        // No newline: hard cut, nothing lost.
        let blob = "é".repeat(CHUNK * 2 + 5);
        let parts = chunks(&blob);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts.concat(), blob);
    }

    #[test]
    fn topic_links() {
        assert_eq!(topic_link(-1001234567, 77), "https://t.me/c/1234567/77");
    }

    #[test]
    fn update_parsing() {
        let u: Vec<Update> = serde_json::from_value(json!([
            {"update_id": 1, "message": {"message_id": 5, "from": {"id": 42},
                "chat": {"id": -100, "type": "supergroup", "is_forum": true},
                "message_thread_id": 7, "is_topic_message": true, "text": "hi",
                "reply_to_message": {"message_id": 4, "chat": {"id": -100}}}},
            {"update_id": 2, "callback_query": {"id": "c", "from": {"id": 42}, "data": "p:1:0",
                "message": {"message_id": 9, "chat": {"id": 42, "type": "private"}}}},
            {"update_id": 3, "edited_message": {}}
        ]))
        .unwrap();
        let m = u[0].message.as_ref().unwrap();
        assert!(m.chat.is_forum && m.is_topic_message);
        assert_eq!(m.reply_to_message.as_ref().unwrap().message_id, 4);
        assert_eq!(u[1].callback_query.as_ref().unwrap().from.id, 42);
        assert!(u[2].message.is_none() && u[2].callback_query.is_none());
    }
}
