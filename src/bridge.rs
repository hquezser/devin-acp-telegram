//! The bridge: Telegram updates → ACP calls, ACP notifications →
//! Telegram messages.
//!
//! - text → `session/new` / `session/prompt` (Devin folds a prompt sent
//!   mid-turn into the running turn; both requests resolve at its end);
//! - `session/update` → final answer (notifies), interim text (silent),
//!   one status message edited in place (tool, plan, mode, context);
//! - `session/request_permission` → inline buttons; the agent waits;
//! - /stop → `session/cancel`, /mode → `session/set_mode`,
//!   /resume → `session/list` + `session/load`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tracing::{info, warn};

use crate::acp::{Acp, Incoming};
use crate::config::Config;
use crate::i18n::Tr;
use crate::state::{self, Session, State};
use crate::telegram::{self, Bot, Dest, Message, Update};

/// Minimum interval between two edits of a session's status message.
const STATUS_EVERY: Duration = Duration::from_millis(2500);
/// Sessions offered by /resume.
const RESUME_MAX: usize = 8;
/// Models offered as buttons by /model.
const MODEL_BUTTONS: usize = 12;

trait LockOk<T> {
    fn lock_ok(&self) -> MutexGuard<'_, T>;
}

impl<T> LockOk<T> for Mutex<T> {
    /// A panicked holder must not take the whole bridge down.
    fn lock_ok(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Runtime state of a session (not persisted).
#[derive(Default)]
struct Live {
    /// Agent text accumulated since the last message.
    buf: String,
    /// Prompts sent whose turn has not ended.
    inflight: u32,
    tools: HashMap<String, String>,
    activity: String,
    plan: Vec<(String, String)>,
    mode: String,
    /// Current model id and the agent's model list (id, display name).
    model: String,
    models: Vec<(String, String)>,
    /// Thinking effort (`thought_level`: medium/high/max) and its choices.
    effort: String,
    efforts: Vec<String>,
    used: u64,
    size: u64,
    /// Pending permission (shown in the status).
    waiting: Option<String>,
    stopped: bool,
    /// `session/load` in progress: replayed history is ignored.
    loading: bool,
    dirty: bool,
}

impl Live {
    /// Mode and model from a `session/new`/`session/load` result or a
    /// `config_option_update` (`configOptions`, ids `mode` and `model`).
    fn apply(&mut self, v: &Value) {
        if let Some(m) = v["modes"]["currentModeId"].as_str() {
            self.mode = m.to_string();
        }
        for opt in v["configOptions"].as_array().into_iter().flatten() {
            let current = opt["currentValue"].as_str().unwrap_or("").to_string();
            match opt["id"].as_str() {
                Some("mode") if !current.is_empty() => self.mode = current,
                Some("thought_level") => {
                    self.effort = current;
                    self.efforts = opt["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|o| o["value"].as_str().map(str::to_string))
                        .collect();
                }
                Some("model") => {
                    self.model = current;
                    self.models = opt["options"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|o| {
                            let id = o["value"].as_str()?.to_string();
                            let name = o["name"].as_str().unwrap_or(&id).to_string();
                            Some((id, name))
                        })
                        .collect();
                }
                _ => {}
            }
        }
    }

    /// Display name with the effort, like Devin's own picker: `SWE-2 Max`.
    fn model_name(&self) -> String {
        let name = self
            .models
            .iter()
            .find(|(id, _)| *id == self.model)
            .map_or_else(|| self.model.clone(), |(_, n)| n.clone());
        let mut effort = self.effort.chars();
        match effort.next() {
            Some(c) => format!("{name} {}{}", c.to_uppercase(), effort.as_str()),
            None => name,
        }
    }
}

/// `opus 5.5`, `Sonnet`, `gpt-6-sol`… → matching models, best first:
/// an exact id/name, else the models whose display name *ends* with the
/// query, else those containing it as is,
/// else those containing every query word. Dots, spaces and underscores
/// count as dashes.
fn match_models(models: &[(String, String)], query: &str) -> Vec<(String, String)> {
    let norm = |s: &str| s.to_lowercase().replace(['.', ' ', '_'], "-");
    let q = norm(query.trim());
    if q.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = q.split('-').filter(|w| !w.is_empty()).collect();
    let fits = |tier: u8, id: &str, name: &str| match tier {
        0 => id == q || name == q,
        // Display name only: Fusion ids end with their sidekick's id.
        1 => name.ends_with(&q),
        2 => id.contains(&q) || name.contains(&q),
        _ => words.iter().all(|w| id.contains(w) || name.contains(w)),
    };
    (0..4)
        .map(|tier| {
            models
                .iter()
                .filter(|(id, name)| fits(tier, &norm(id), &norm(name)))
                .cloned()
                .collect::<Vec<_>>()
        })
        .find(|m| !m.is_empty())
        .unwrap_or_default()
}

struct Perm {
    session: String,
    option_ids: Vec<String>,
    labels: Vec<String>,
    text: String,
    /// Question message, once sent.
    msg: Option<(i64, i64)>,
    tx: oneshot::Sender<Option<String>>,
}

struct Offer {
    session: String,
    cwd: String,
    title: String,
    locked_pid: Option<String>,
}

/// Telegram sends, serialized to keep message order.
enum Out {
    Text {
        sid: String,
        text: String,
        silent: bool,
    },
    Status {
        sid: String,
        fresh: bool,
    },
    Ask {
        qid: u64,
    },
    Notice {
        to: Dest,
        text: String,
    },
    Rename {
        sid: String,
    },
}

#[derive(Debug, PartialEq)]
enum Command_ {
    Help,
    New(String),
    Resume(String),
    Sessions,
    Projects,
    Stop,
    Mode(String),
    Model(String),
    Effort(String),
    Close,
    Caffeine(String),
    Link,
    Unlink,
}

/// `/stop`, `/stop@MyBot`, `/mode plan`… → bridge command; other slash
/// commands belong to Devin.
fn command(text: &str) -> Option<Command_> {
    let rest = text.trim().strip_prefix('/')?;
    let (head, arg) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let name = head.split('@').next().unwrap_or(head).to_lowercase();
    let arg = arg.trim().to_string();
    Some(match name.as_str() {
        "help" | "start" | "aide" => Command_::Help,
        "new" | "nouvelle" => Command_::New(arg),
        "resume" | "reprendre" => Command_::Resume(arg),
        "sessions" | "status" | "etat" => Command_::Sessions,
        "projects" | "projets" => Command_::Projects,
        "stop" => Command_::Stop,
        "mode" => Command_::Mode(arg),
        "model" | "modele" | "modèle" => Command_::Model(arg),
        "effort" | "reflexion" | "réflexion" | "thinking" => Command_::Effort(arg),
        "close" | "fermer" => Command_::Close,
        "caffeine" | "cafe" | "café" => Command_::Caffeine(arg),
        "link" | "lier" => Command_::Link,
        "unlink" | "delier" | "délier" => Command_::Unlink,
        _ => return None,
    })
}

/// ACP session mode from a friendly name.
fn mode_id(arg: &str) -> Option<&'static str> {
    Some(match arg.trim().to_lowercase().as_str() {
        "code" | "accept-edits" | "edit" => "accept-edits",
        "smart" | "auto" => "smart",
        "ask" => "ask",
        "plan" => "plan",
        "bypass" | "yolo" => "bypass",
        _ => return None,
    })
}

/// Session title from its first prompt.
fn title_of(text: &str) -> String {
    let line = text.lines().next().unwrap_or("").trim();
    let mut t: String = line.chars().take(48).collect();
    if line.chars().count() > 48 {
        t.push('…');
    }
    t
}

/// Button label of an ACP permission option.
fn option_label(o: &Value) -> String {
    let mark = match o["kind"].as_str().unwrap_or("") {
        "allow_once" => "✅",
        "allow_always" => "♾️",
        "reject_once" | "reject_always" => "⛔",
        _ => "•",
    };
    format!("{mark} {}", o["name"].as_str().unwrap_or("?"))
}

pub struct Bridge {
    cfg: Config,
    tr: Tr,
    bot: Bot,
    acp: Arc<Acp>,
    host: String,
    state: Mutex<State>,
    live: Mutex<HashMap<String, Live>>,
    perms: Mutex<HashMap<u64, Perm>>,
    answered: Mutex<HashSet<u64>>,
    offers: Mutex<HashMap<u64, Offer>>,
    /// /model buttons: token → (session, model id).
    model_offers: Mutex<HashMap<u64, (String, String)>>,
    next_id: AtomicU64,
    caffeine: Mutex<Option<Child>>,
    out: mpsc::UnboundedSender<Out>,
}

impl Bridge {
    /// Builds the bridge and starts its loops (Telegram polling, agent
    /// events, outbox, status ticker) in the returned set.
    pub fn start(
        cfg: Config,
        acp: Arc<Acp>,
        incoming: mpsc::UnboundedReceiver<Incoming>,
        host: String,
    ) -> (Arc<Bridge>, JoinSet<()>) {
        let (out, out_rx) = mpsc::unbounded_channel();
        let bridge = Arc::new(Bridge {
            tr: Tr(cfg.lang),
            bot: Bot::new(&cfg.api_url, &cfg.token),
            state: Mutex::new(State::load(&cfg.state_dir)),
            cfg,
            acp,
            host,
            live: Mutex::default(),
            perms: Mutex::default(),
            answered: Mutex::default(),
            offers: Mutex::default(),
            model_offers: Mutex::default(),
            next_id: AtomicU64::new(1),
            caffeine: Mutex::default(),
            out,
        });
        let mut tasks = JoinSet::new();
        tasks.spawn(bridge.clone().run_outbox(out_rx));
        tasks.spawn(bridge.clone().run_incoming(incoming));
        tasks.spawn(bridge.clone().run_ticker());
        (bridge, tasks)
    }

    fn emit(&self, o: Out) {
        self.out.send(o).ok();
    }

    fn next(&self) -> u64 {
        self.next_id.fetch_add(1, Ordering::Relaxed)
    }

    fn save(&self) {
        self.state.lock_ok().save();
    }

    fn session(&self, sid: &str) -> Option<Session> {
        self.state.lock_ok().sessions.get(sid).cloned()
    }

    fn owners(&self) -> Vec<Dest> {
        self.cfg
            .allowed_users
            .iter()
            .map(|&u| Dest {
                chat: u,
                thread: None,
            })
            .collect()
    }

    /// Announces itself, registers the command menu, resumes the sessions
    /// left open, applies caffeine, then starts Telegram polling.
    pub async fn startup(self: &Arc<Self>, tasks: &mut JoinSet<()>, initial: Option<String>) {
        let caffeine = self.cfg.caffeine && self.set_caffeine(true);
        self.register_commands().await;
        let msg = self.tr.online(&self.host, &self.cfg.cwd, caffeine);
        for to in self.owners() {
            if let Err(e) = self.bot.send(to, &msg, true, None).await {
                warn!(error = %e, "announce failed (has the user started the bot?)");
            }
        }
        let open: Vec<(String, String)> = self
            .state
            .lock_ok()
            .sessions
            .iter()
            .map(|(id, s)| (id.clone(), s.cwd.clone()))
            .collect();
        for (sid, cwd) in open {
            if let Err(e) = self.load(&sid, &cwd).await {
                warn!(error = %e, session = sid, "session not resumed");
                if let Some(s) = self.session(&sid) {
                    self.emit(Out::Notice {
                        to: s.dest(),
                        text: self.tr.not_resumed(&e.to_string()),
                    });
                }
                self.state.lock_ok().remove(&sid);
                self.save();
            }
        }
        tasks.spawn(self.clone().run_updates());
        if let Some(p) = initial.filter(|p| !p.trim().is_empty())
            && let Some(&owner) = self.cfg.allowed_users.first()
        {
            let to = Dest {
                chat: owner,
                thread: None,
            };
            tokio::spawn(self.clone().new_session(to, p));
        }
    }

    pub async fn shutdown(&self, error: Option<&str>) {
        let msg = self.tr.offline(error);
        for to in self.owners() {
            self.bot.send(to, &msg, true, None).await.ok();
        }
    }

    async fn register_commands(&self) {
        let fr = matches!(self.cfg.lang, crate::i18n::Lang::Fr);
        let list = [
            (
                "new",
                "New session: /new [project:] text",
                "Nouvelle session : /new [projet:] texte",
            ),
            (
                "resume",
                "Resume a recent Devin session",
                "Reprendre une session Devin récente",
            ),
            (
                "stop",
                "Interrupt the current turn",
                "Interrompre le tour en cours",
            ),
            (
                "mode",
                "code | smart | ask | plan | bypass",
                "code | smart | ask | plan | bypass",
            ),
            (
                "model",
                "Switch model: /model [name]",
                "Changer de modèle : /model [nom]",
            ),
            (
                "effort",
                "Thinking effort: medium | high | max",
                "Niveau de réflexion : medium | high | max",
            ),
            (
                "sessions",
                "Bridge and session status",
                "État du pont et des sessions",
            ),
            ("projects", "Project aliases", "Alias de projets"),
            ("close", "End the session", "Terminer la session"),
            (
                "caffeine",
                "Keep the Mac awake: on | off",
                "Garder le Mac éveillé : on | off",
            ),
            (
                "link",
                "Use this group: one topic per session",
                "Utiliser ce groupe : un sujet par session",
            ),
            (
                "unlink",
                "Stop using this group",
                "Ne plus utiliser ce groupe",
            ),
            ("help", "Help", "Aide"),
        ];
        let commands: Vec<Value> = list
            .iter()
            .map(|(c, en, f)| json!({"command": c, "description": if fr { f } else { en }}))
            .collect();
        self.bot
            .try_call("setMyCommands", json!({"commands": commands}))
            .await;
    }

    // -----------------------------------------------------------------
    // Outbox

    /// Prefix of messages in a shared chat (several sessions, no topic).
    fn header(s: &Session, text: &str) -> String {
        if s.thread.is_some() {
            text.to_string()
        } else {
            format!("🤖 {}\n{text}", s.title)
        }
    }

    async fn run_outbox(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Out>) {
        while let Some(o) = rx.recv().await {
            if let Err(e) = self.deliver(o).await {
                warn!(error = %e, "telegram send failed");
            }
        }
    }

    async fn deliver(&self, o: Out) -> anyhow::Result<()> {
        match o {
            Out::Text { sid, text, silent } => {
                let Some(s) = self.session(&sid) else {
                    return Ok(());
                };
                if text.trim().is_empty() {
                    return Ok(());
                }
                let ids = self
                    .bot
                    .send_long(s.dest(), &Self::header(&s, &text), silent)
                    .await?;
                let mut st = self.state.lock_ok();
                for id in ids {
                    st.index_msg(s.chat, id, &sid);
                }
                st.touch(&sid);
                st.save();
            }
            Out::Status { sid, fresh } => {
                let Some(s) = self.session(&sid) else {
                    return Ok(());
                };
                let text = {
                    let mut live = self.live.lock_ok();
                    let Some(l) = live.get_mut(&sid) else {
                        return Ok(());
                    };
                    l.dirty = false;
                    self.status_text(l)
                };
                let text: String = Self::header(&s, &text)
                    .chars()
                    .take(telegram::CHUNK)
                    .collect();
                let mut msg = s.status_msg.filter(|_| !fresh);
                if let Some(m) = msg
                    && let Err(e) = self.bot.edit(s.chat, m, &text).await
                {
                    warn!(error = %e, "status not edited, sending a new one");
                    msg = None;
                }
                if msg.is_none() {
                    let id = self.bot.send(s.dest(), &text, true, None).await?;
                    let mut st = self.state.lock_ok();
                    st.index_msg(s.chat, id, &sid);
                    if let Some(s) = st.sessions.get_mut(&sid) {
                        s.status_msg = Some(id);
                    }
                    st.save();
                }
            }
            Out::Ask { qid } => {
                let Some((sid, text, labels)) = self
                    .perms
                    .lock_ok()
                    .get(&qid)
                    .map(|p| (p.session.clone(), p.text.clone(), p.labels.clone()))
                else {
                    return Ok(());
                };
                let Some(s) = self.session(&sid) else {
                    return Ok(());
                };
                let keyboard: Vec<Value> = labels
                    .iter()
                    .enumerate()
                    .map(|(i, l)| json!([{"text": l, "callback_data": format!("p:{qid}:{i}")}]))
                    .collect();
                let text = Self::header(&s, &text);
                // The agent waits for this answer: keep trying.
                let id = loop {
                    match self
                        .bot
                        .send(
                            s.dest(),
                            &text,
                            false,
                            Some(json!({"inline_keyboard": keyboard})),
                        )
                        .await
                    {
                        Ok(id) => break id,
                        Err(e) => {
                            warn!(error = %e, "permission prompt not sent, retrying");
                            if !self.perms.lock_ok().contains_key(&qid) {
                                return Ok(());
                            }
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        }
                    }
                };
                if let Some(p) = self.perms.lock_ok().get_mut(&qid) {
                    p.msg = Some((s.chat, id));
                }
                let mut st = self.state.lock_ok();
                st.index_msg(s.chat, id, &sid);
                st.save();
            }
            Out::Notice { to, text } => {
                self.bot.send_long(to, &text, false).await?;
            }
            Out::Rename { sid } => {
                if let Some(s) = self.session(&sid)
                    && let Some(thread) = s.thread
                {
                    self.bot
                        .try_call(
                            "editForumTopic",
                            json!({"chat_id": s.chat, "message_thread_id": thread,
                                   "name": format!("🤖 {}", s.title)}),
                        )
                        .await;
                }
            }
        }
        Ok(())
    }

    fn status_text(&self, l: &Live) -> String {
        let tr = self.tr;
        let mut out = if let Some(w) = &l.waiting {
            tr.st_waiting(w)
        } else if l.inflight > 0 {
            if l.activity.is_empty() {
                tr.st_working().to_string()
            } else {
                format!("{} · {}", tr.st_working(), l.activity)
            }
        } else if l.stopped {
            tr.st_stopped().to_string()
        } else {
            tr.st_idle().to_string()
        };
        if !l.plan.is_empty() {
            out.push('\n');
            for (status, content) in &l.plan {
                let mark = match status.as_str() {
                    "completed" => "✅",
                    "in_progress" => "▶️",
                    _ => "▫️",
                };
                out.push_str(&format!("\n{mark} {content}"));
            }
        }
        let mut foot = Vec::new();
        if !l.model.is_empty() {
            foot.push(l.model_name());
        }
        if !l.mode.is_empty() {
            foot.push(format!("mode {}", l.mode));
        }
        if l.size > 0 {
            foot.push(tr.st_context(l.used / 1000, l.size / 1000));
        }
        if !foot.is_empty() {
            out.push_str(&format!("\n\n⚙️ {}", foot.join(" · ")));
        }
        out
    }

    /// Dirty statuses pushed at most every `STATUS_EVERY`.
    async fn run_ticker(self: Arc<Self>) {
        let mut tick = tokio::time::interval(STATUS_EVERY);
        loop {
            tick.tick().await;
            let dirty: Vec<String> = self
                .live
                .lock_ok()
                .iter_mut()
                .filter(|(_, l)| l.dirty && !l.loading)
                .map(|(id, l)| {
                    l.dirty = false;
                    id.clone()
                })
                .collect();
            for sid in dirty {
                self.emit(Out::Status { sid, fresh: false });
            }
        }
    }

    // -----------------------------------------------------------------
    // Agent → bridge

    async fn run_incoming(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Incoming>) {
        while let Some(i) = rx.recv().await {
            match i {
                Incoming::Update(p) => self.on_update(&p),
                Incoming::Request { id, method, params } => {
                    tokio::spawn(self.clone().on_request(id, method, params));
                }
            }
        }
    }

    fn on_update(&self, p: &Value) {
        let Some(sid) = p["sessionId"].as_str() else {
            return;
        };
        let u = &p["update"];
        let mut flush = None;
        let mut rename = None;
        {
            let mut live = self.live.lock_ok();
            let Some(l) = live.get_mut(sid) else { return };
            if l.loading {
                return;
            }
            match u["sessionUpdate"].as_str().unwrap_or("") {
                "agent_message_chunk" => {
                    if let Some(t) = u["content"]["text"].as_str() {
                        l.buf.push_str(t);
                    }
                }
                "tool_call" => {
                    let title = u["title"].as_str().unwrap_or("tool").to_string();
                    if let Some(id) = u["toolCallId"].as_str() {
                        l.tools.insert(id.to_string(), title.clone());
                    }
                    l.activity = title;
                    l.dirty = true;
                    // Agent text before a tool call: interim message.
                    if !l.buf.trim().is_empty() {
                        flush = Some(std::mem::take(&mut l.buf));
                    }
                }
                "tool_call_update" => {
                    let id = u["toolCallId"].as_str().unwrap_or("");
                    if let Some(t) = u["title"].as_str() {
                        l.tools.insert(id.to_string(), t.to_string());
                        l.activity = t.to_string();
                        l.dirty = true;
                    }
                    if u["status"] == "failed" {
                        let t = l.tools.get(id).cloned().unwrap_or_default();
                        l.activity = self.tr.st_failed(&t);
                        l.dirty = true;
                    }
                }
                "plan" => {
                    l.plan = u["entries"]
                        .as_array()
                        .map(|a| {
                            a.iter()
                                .map(|e| {
                                    (
                                        e["status"].as_str().unwrap_or("").to_string(),
                                        e["content"].as_str().unwrap_or("").to_string(),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    l.dirty = true;
                }
                "config_option_update" => {
                    l.apply(&json!({"configOptions": u["configOptions"]}));
                    l.dirty = true;
                }
                "current_mode_update" => {
                    l.mode = u["currentModeId"].as_str().unwrap_or("").to_string();
                    l.dirty = true;
                }
                "usage_update" => {
                    l.used = u["used"].as_u64().unwrap_or(l.used);
                    l.size = u["size"].as_u64().unwrap_or(l.size);
                }
                "session_info_update" => {
                    if let Some(t) = u["title"].as_str() {
                        rename = Some(t.to_string());
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = rename {
            // Title refined by Devin: the topic follows.
            let mut st = self.state.lock_ok();
            if let Some(s) = st.sessions.get_mut(sid) {
                let title = format!("[{}] {}", self.cfg.tag_for(&s.cwd), title_of(&t));
                if title != s.title {
                    s.title = title;
                    st.save();
                    self.emit(Out::Rename {
                        sid: sid.to_string(),
                    });
                }
            }
        }
        if let Some(text) = flush {
            self.emit(Out::Text {
                sid: sid.to_string(),
                text,
                silent: true,
            });
        }
    }

    async fn on_request(self: Arc<Self>, id: Value, method: String, params: Value) {
        if method != "session/request_permission" {
            // Nothing else is advertised in clientCapabilities.
            self.acp
                .respond_error(id, -32601, &format!("method not supported: {method}"))
                .await;
            return;
        }
        let result = match self.permission(&params).await {
            Some(option) => json!({"outcome": {"outcome": "selected", "optionId": option}}),
            None => json!({"outcome": {"outcome": "cancelled"}}),
        };
        self.acp.respond(id, result).await;
    }

    /// Permission prompt → buttons; waits like the terminal does. `None`
    /// = cancelled (/stop).
    async fn permission(&self, p: &Value) -> Option<String> {
        let sid = p["sessionId"].as_str()?.to_string();
        let tc = &p["toolCall"];
        let tool_id = tc["toolCallId"].as_str().unwrap_or("");
        let title = self
            .live
            .lock_ok()
            .get(&sid)
            .and_then(|l| l.tools.get(tool_id).cloned())
            .or_else(|| tc["title"].as_str().map(str::to_string))
            .unwrap_or_else(|| "action".to_string());
        let cmd = tc["_meta"]["cognition.ai/editableCommand"]
            .as_str()
            .or_else(|| tc["rawInput"]["command"].as_str());
        let mut text = self.tr.permission(&title);
        if let Some(c) = cmd {
            text.push_str(&format!("\n$ {c}"));
        }
        let opts = p["options"].as_array().cloned().unwrap_or_default();
        let opts: Vec<Value> = opts.into_iter().take(8).collect();
        let qid = self.next();
        let (tx, rx) = oneshot::channel();
        self.perms.lock_ok().insert(
            qid,
            Perm {
                session: sid.clone(),
                option_ids: opts
                    .iter()
                    .map(|o| o["optionId"].as_str().unwrap_or("").to_string())
                    .collect(),
                labels: opts.iter().map(option_label).collect(),
                text,
                msg: None,
                tx,
            },
        );
        if let Some(l) = self.live.lock_ok().get_mut(&sid) {
            l.waiting = Some(cmd.map(str::to_string).unwrap_or(title));
        }
        self.emit(Out::Status {
            sid: sid.clone(),
            fresh: false,
        });
        self.emit(Out::Ask { qid });
        let choice = rx.await.ok().flatten();
        if let Some(l) = self.live.lock_ok().get_mut(&sid) {
            l.waiting = None;
            l.dirty = true;
        }
        choice
    }

    // -----------------------------------------------------------------
    // Sessions

    /// Where a new session lives: a topic of the linked group if any
    /// (falls back to `origin`).
    async fn place(&self, origin: Dest, title: &str) -> Dest {
        let group = self.state.lock_ok().group;
        let Some(group) = group else { return origin };
        match self
            .bot
            .call(
                "createForumTopic",
                &json!({"chat_id": group, "name": format!("🤖 {title}").chars().take(128).collect::<String>()}),
            )
            .await
        {
            Ok(r) => match r["message_thread_id"].as_i64() {
                Some(t) => Dest {
                    chat: group,
                    thread: Some(t),
                },
                None => origin,
            },
            Err(e) => {
                warn!(error = %e, "topic not created (is the bot an admin allowed to manage topics?)");
                origin
            }
        }
    }

    async fn new_session(self: Arc<Self>, origin: Dest, text: String) {
        let (cwd, prompt) = match self.cfg.project(&text) {
            Ok(p) => p,
            Err(path) => {
                self.emit(Out::Notice {
                    to: origin,
                    text: self.tr.unknown_folder(&path),
                });
                return;
            }
        };
        let r = self
            .acp
            .request(
                "session/new",
                // Devin only persists a session after its first prompt:
                // ask for it now so an empty session survives a restart.
                json!({"cwd": cwd, "mcpServers": [],
                       "_meta": {"cognition.ai/persistOnCreate": true}}),
            )
            .await;
        let (sid, init) = match &r {
            Ok(v) => match v["sessionId"].as_str() {
                Some(s) => {
                    let mut l = Live::default();
                    l.apply(v);
                    (s.to_string(), l)
                }
                None => {
                    self.emit(Out::Notice {
                        to: origin,
                        text: self.tr.new_failed("no sessionId"),
                    });
                    return;
                }
            },
            Err(e) => {
                self.emit(Out::Notice {
                    to: origin,
                    text: self.tr.new_failed(&e.to_string()),
                });
                return;
            }
        };
        let tag = self.cfg.tag_for(&cwd);
        let title = if prompt.is_empty() {
            format!("[{tag}] {sid}")
        } else {
            format!("[{tag}] {}", title_of(&prompt))
        };
        let dest = self.place(origin, &title).await;
        self.live.lock_ok().insert(sid.clone(), init);
        self.adopt(&sid, &title, &cwd, dest);
        if dest != origin
            && let Some(t) = dest.thread
        {
            self.emit(Out::Notice {
                to: origin,
                text: self
                    .tr
                    .new_in_topic(&title, &telegram::topic_link(dest.chat, t)),
            });
        }
        if let Some(m) = self.cfg.mode.clone() {
            self.set_mode(&sid, &m).await;
        }
        if prompt.is_empty() {
            self.emit(Out::Text {
                sid,
                text: self.tr.opened_empty(&cwd),
                silent: false,
            });
        } else {
            self.prompt(sid, prompt).await;
        }
    }

    /// Records a session (persisted) and its runtime state.
    fn adopt(&self, sid: &str, title: &str, cwd: &str, dest: Dest) {
        self.live.lock_ok().entry(sid.to_string()).or_default();
        let mut st = self.state.lock_ok();
        st.sessions.insert(
            sid.to_string(),
            Session {
                title: title.to_string(),
                cwd: cwd.to_string(),
                chat: dest.chat,
                thread: dest.thread,
                status_msg: None,
                updated: state::now(),
            },
        );
        st.save();
    }

    /// `session/load`, ignoring the replayed history.
    async fn load(&self, sid: &str, cwd: &str) -> anyhow::Result<()> {
        self.live.lock_ok().insert(
            sid.to_string(),
            Live {
                loading: true,
                ..Default::default()
            },
        );
        let r = self
            .acp
            .request(
                "session/load",
                json!({"sessionId": sid, "cwd": cwd, "mcpServers": []}),
            )
            .await;
        let mut live = self.live.lock_ok();
        match r {
            Ok(v) => {
                if let Some(l) = live.get_mut(sid) {
                    l.loading = false;
                    l.buf.clear();
                    l.apply(&v);
                }
                info!(session = sid, "session resumed");
                Ok(())
            }
            Err(e) => {
                live.remove(sid);
                Err(e)
            }
        }
    }

    async fn prompt(&self, sid: String, text: String) {
        let fresh = {
            let mut live = self.live.lock_ok();
            let Some(l) = live.get_mut(&sid) else { return };
            l.inflight += 1;
            l.stopped = false;
            if l.inflight == 1 {
                l.activity.clear();
            }
            l.inflight == 1
        };
        if fresh {
            self.emit(Out::Status {
                sid: sid.clone(),
                fresh: true,
            });
        }
        let r = self
            .acp
            .request(
                "session/prompt",
                json!({"sessionId": sid, "prompt": [{"type": "text", "text": text}]}),
            )
            .await;
        let (mut out, idle) = {
            let mut live = self.live.lock_ok();
            let Some(l) = live.get_mut(&sid) else { return };
            l.inflight = l.inflight.saturating_sub(1);
            (std::mem::take(&mut l.buf), l.inflight == 0)
        };
        match r {
            Ok(v) => match v["stopReason"].as_str().unwrap_or("end_turn") {
                "end_turn" => {}
                "cancelled" => {
                    if let Some(l) = self.live.lock_ok().get_mut(&sid) {
                        l.stopped = true;
                    }
                    out.push_str("\n\n");
                    out.push_str(self.tr.interrupted());
                }
                other => {
                    out.push_str("\n\n");
                    out.push_str(&self.tr.turn_end(other));
                }
            },
            Err(e) => {
                out.push_str("\n\n");
                out.push_str(&self.tr.devin_error(&e.to_string()));
            }
        }
        self.emit(Out::Text {
            sid: sid.clone(),
            text: out,
            silent: false,
        });
        if idle {
            self.emit(Out::Status { sid, fresh: false });
        }
    }

    /// /model [query]: switch directly on a single match, else buttons.
    async fn model_cmd(self: Arc<Self>, origin: Dest, sid: String, query: String) {
        let (models, current) = match self.live.lock_ok().get(&sid) {
            Some(l) => (l.models.clone(), l.model_name()),
            None => (Vec::new(), String::new()),
        };
        if models.is_empty() {
            return self.emit(Out::Notice {
                to: origin,
                text: self.tr.model_unknown().to_string(),
            });
        }
        let choices = if query.is_empty() {
            models
        } else {
            match match_models(&models, &query).as_slice() {
                [] => {
                    return self.emit(Out::Notice {
                        to: origin,
                        text: self.tr.model_none(&query),
                    });
                }
                [(id, _)] => return self.set_model(&sid, id).await,
                many => many.to_vec(),
            }
        };
        let buttons: Vec<Value> = choices
            .iter()
            .take(MODEL_BUTTONS)
            .map(|(id, name)| {
                let token = self.next();
                self.model_offers
                    .lock_ok()
                    .insert(token, (sid.clone(), id.clone()));
                json!({"text": name, "callback_data": format!("m:{token}")})
            })
            .collect();
        let keyboard: Vec<Value> = buttons.chunks(2).map(|row| json!(row)).collect();
        let more = choices.len().saturating_sub(MODEL_BUTTONS);
        if let Err(e) = self
            .bot
            .send(
                origin,
                &self.tr.model_pick(&current, more),
                false,
                Some(json!({"inline_keyboard": keyboard})),
            )
            .await
        {
            warn!(error = %e, "model list not sent");
        }
    }

    async fn set_model(&self, sid: &str, model: &str) {
        self.set_config(sid, "model", model).await;
    }

    /// `session/set_config_option` (model, thought_level…); the result
    /// carries the updated options.
    async fn set_config(&self, sid: &str, id: &str, value: &str) {
        let model = value;
        let r = self
            .acp
            .request(
                "session/set_config_option",
                json!({"sessionId": sid, "configId": id, "value": value}),
            )
            .await;
        let text = match r {
            Ok(v) => {
                let mut live = self.live.lock_ok();
                match live.get_mut(sid) {
                    Some(l) => {
                        l.apply(&v);
                        l.dirty = true;
                        self.tr.model_set(&l.model_name())
                    }
                    None => self.tr.model_set(model),
                }
            }
            Err(e) => self.tr.model_refused(model, &e.to_string()),
        };
        self.emit(Out::Text {
            sid: sid.to_string(),
            text,
            silent: true,
        });
    }

    async fn set_mode(&self, sid: &str, mode: &str) {
        let r = self
            .acp
            .request(
                "session/set_mode",
                json!({"sessionId": sid, "modeId": mode}),
            )
            .await;
        let text = match r {
            Ok(_) => self.tr.mode_set(mode),
            Err(e) => self.tr.mode_refused(mode, &e.to_string()),
        };
        self.emit(Out::Text {
            sid: sid.to_string(),
            text,
            silent: true,
        });
    }

    /// /stop: cancels the turn and any pending permission.
    async fn cancel(&self, sid: &str) {
        let pending: Vec<u64> = self
            .perms
            .lock_ok()
            .iter()
            .filter(|(_, p)| p.session == sid)
            .map(|(q, _)| *q)
            .collect();
        for q in pending {
            if let Some(p) = self.perms.lock_ok().remove(&q) {
                p.tx.send(None).ok();
            }
        }
        if let Err(e) = self
            .acp
            .notify("session/cancel", json!({"sessionId": sid}))
            .await
        {
            warn!(error = %e, "session/cancel not sent");
        }
    }

    fn set_caffeine(&self, on: bool) -> bool {
        let mut c = self.caffeine.lock_ok();
        if on && c.is_none() {
            // `-w <pid>`: caffeinate ends with us, even on SIGKILL.
            match Command::new("caffeinate")
                .args(["-ims", "-w", &std::process::id().to_string()])
                .kill_on_drop(true)
                .spawn()
            {
                Ok(child) => *c = Some(child),
                Err(e) => warn!(error = %e, "caffeinate unavailable (macOS only)"),
            }
        } else if !on && let Some(mut child) = c.take() {
            child.start_kill().ok();
        }
        c.is_some()
    }

    // -----------------------------------------------------------------
    // Telegram → bridge

    async fn run_updates(self: Arc<Self>) {
        let mut backoff = 2;
        loop {
            let offset = self.state.lock_ok().offset;
            match self.bot.updates(offset).await {
                Ok(list) => {
                    backoff = 2;
                    for u in list {
                        {
                            let mut st = self.state.lock_ok();
                            st.offset = st.offset.max(u.update_id + 1);
                        }
                        self.on_telegram(u).await;
                    }
                    self.save();
                }
                Err(e) => {
                    warn!(error = %e, "getUpdates failed, retrying");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    }

    /// Chats the bridge listens to: owners' private chats, the linked
    /// group, and any group still hosting a session.
    fn chat_allowed(&self, chat: i64, from: i64) -> bool {
        if chat == from {
            return true;
        }
        let st = self.state.lock_ok();
        st.group == Some(chat) || st.sessions.values().any(|s| s.chat == chat)
    }

    async fn on_telegram(self: &Arc<Self>, u: Update) {
        if let Some(q) = u.callback_query {
            if !self.cfg.is_allowed(q.from.id) {
                return;
            }
            let (Some(data), Some(m)) = (q.data, q.message) else {
                return;
            };
            self.on_button(&q.id, &m, &data).await;
            return;
        }
        let Some(m) = u.message else { return };
        let Some(from) = m.from.as_ref().map(|f| f.id) else {
            return;
        };
        if !self.cfg.is_allowed(from) {
            info!(
                user = from,
                chat = m.chat.id,
                "message from a user not allowed, ignored"
            );
            return;
        }
        let Some(text) = m.text.as_deref().map(str::trim).filter(|t| !t.is_empty()) else {
            return;
        };
        let text = text.to_string();
        let cmd = command(&text);
        if !self.chat_allowed(m.chat.id, from) {
            if cmd == Some(Command_::Link) {
                self.link(&m).await;
            }
            return;
        }
        let origin = Dest {
            chat: m.chat.id,
            thread: m.message_thread_id.filter(|_| m.is_topic_message),
        };
        let in_group = m.chat.id != from;
        // Topic → its session; else a reply to a session message.
        let mut target = None;
        if let Some(t) = origin.thread {
            target = self.state.lock_ok().session_of_thread(m.chat.id, t);
        }
        if target.is_none()
            && let Some(r) = &m.reply_to_message
        {
            target = self.state.lock_ok().session_of_msg(m.chat.id, r.message_id);
        }
        let reply = |text: String| {
            self.emit(Out::Notice { to: origin, text });
        };
        // Named session, else the most recent one (not from a group's
        // General topic: there, text means a new session).
        let pick = |allow_latest: bool| -> Option<String> {
            target.clone().or_else(|| {
                allow_latest
                    .then(|| self.state.lock_ok().latest())
                    .flatten()
            })
        };
        match cmd {
            Some(Command_::Help) => reply(self.tr.help().to_string()),
            Some(Command_::New(arg)) => {
                if arg.is_empty() {
                    reply(self.tr.new_usage().to_string());
                } else {
                    tokio::spawn(self.clone().new_session(origin, arg));
                }
            }
            Some(Command_::Resume(arg)) => {
                tokio::spawn(self.clone().resume_list(origin, arg));
            }
            Some(Command_::Sessions) => reply(self.sessions_text()),
            Some(Command_::Projects) => reply(self.tr.projects(&self.cfg.cwd, &self.cfg.projects)),
            Some(Command_::Effort(arg)) => {
                let Some(sid) = pick(true) else {
                    return reply(self.tr.no_session().to_string());
                };
                let efforts = self
                    .live
                    .lock_ok()
                    .get(&sid)
                    .map(|l| l.efforts.clone())
                    .unwrap_or_default();
                let level = arg.to_lowercase();
                if efforts.contains(&level) {
                    let me = self.clone();
                    tokio::spawn(async move {
                        me.set_config(&sid, "thought_level", &level).await;
                    });
                } else {
                    reply(self.tr.effort_usage(&efforts.join(" | ")));
                }
            }
            Some(Command_::Model(arg)) => {
                let Some(sid) = pick(true) else {
                    return reply(self.tr.no_session().to_string());
                };
                tokio::spawn(self.clone().model_cmd(origin, sid, arg));
            }
            Some(Command_::Caffeine(arg)) => {
                let on = match arg.to_lowercase().as_str() {
                    "" | "on" | "1" => true,
                    "off" | "0" => false,
                    _ => return reply(self.tr.caffeine_usage().to_string()),
                };
                let on = self.set_caffeine(on);
                reply(self.tr.caffeine(on).to_string());
            }
            Some(Command_::Link) => self.link(&m).await,
            Some(Command_::Unlink) => {
                let mut st = self.state.lock_ok();
                if in_group && st.group == Some(m.chat.id) {
                    st.group = None;
                    st.save();
                    drop(st);
                    reply(self.tr.unlinked().to_string());
                } else {
                    drop(st);
                    reply(self.tr.unlink_where().to_string());
                }
            }
            Some(c @ (Command_::Stop | Command_::Close | Command_::Mode(_))) => {
                let Some(sid) = pick(true) else {
                    return reply(self.tr.no_session().to_string());
                };
                let title = self.session(&sid).map(|s| s.title).unwrap_or_default();
                match c {
                    Command_::Stop => {
                        self.cancel(&sid).await;
                        reply(self.tr.stop_requested(&title));
                    }
                    Command_::Close => {
                        self.cancel(&sid).await;
                        self.close(&sid).await;
                    }
                    Command_::Mode(arg) => match mode_id(&arg) {
                        Some(mode) => {
                            let me = self.clone();
                            tokio::spawn(async move { me.set_mode(&sid, mode).await });
                        }
                        None => reply(self.tr.modes_usage().to_string()),
                    },
                    _ => unreachable!(),
                }
            }
            None => {
                let Some(sid) = pick(!in_group) else {
                    tokio::spawn(self.clone().new_session(origin, text));
                    return;
                };
                let s = self.session(&sid);
                self.bot
                    .try_call(
                        "setMessageReaction",
                        json!({"chat_id": m.chat.id, "message_id": m.message_id,
                               "reaction": [{"type": "emoji", "emoji": "👀"}]}),
                    )
                    .await;
                if let Some(s) = &s
                    && s.dest() != origin
                {
                    let link = s
                        .thread
                        .map(|t| format!("\n{}", telegram::topic_link(s.chat, t)))
                        .unwrap_or_default();
                    reply(format!("{}{link}", self.tr.routed(&s.title)));
                }
                self.state.lock_ok().touch(&sid);
                let me = self.clone();
                tokio::spawn(async move { me.prompt(sid, text).await });
            }
        }
    }

    async fn link(&self, m: &Message) {
        if m.chat.kind == "private" {
            return self.emit(Out::Notice {
                to: Dest {
                    chat: m.chat.id,
                    thread: None,
                },
                text: self.tr.unlink_where().replace("/unlink", "/link"),
            });
        }
        {
            let mut st = self.state.lock_ok();
            st.group = Some(m.chat.id);
            st.save();
        }
        self.emit(Out::Notice {
            to: Dest {
                chat: m.chat.id,
                thread: m.message_thread_id.filter(|_| m.is_topic_message),
            },
            text: self.tr.linked(m.chat.is_forum).to_string(),
        });
    }

    async fn close(&self, sid: &str) {
        let Some(s) = self.session(sid) else { return };
        self.bot
            .send(s.dest(), &Self::header(&s, self.tr.closed()), false, None)
            .await
            .ok();
        if let Some(t) = s.thread {
            self.bot
                .try_call(
                    "closeForumTopic",
                    json!({"chat_id": s.chat, "message_thread_id": t}),
                )
                .await;
        }
        self.acp
            .request("session/close", json!({"sessionId": sid}))
            .await
            .ok();
        self.live.lock_ok().remove(sid);
        let mut st = self.state.lock_ok();
        st.remove(sid);
        st.save();
    }

    fn sessions_text(&self) -> String {
        let st = self.state.lock_ok();
        let live = self.live.lock_ok();
        let mut out = self.tr.bridge_line(
            &self.host,
            self.caffeine.lock_ok().is_some(),
            st.sessions.len(),
        );
        let mut list: Vec<_> = st.sessions.iter().collect();
        list.sort_by_key(|(_, s)| std::cmp::Reverse(s.updated));
        for (id, s) in list {
            let status = live
                .get(id)
                .map(|l| self.status_text(l))
                .unwrap_or_default();
            let status = status.lines().next().unwrap_or("");
            out.push_str(&format!("\n\n• {} ({id})\n  {status}", s.title));
        }
        out
    }

    /// /resume: recent sessions as buttons (optionally one project).
    async fn resume_list(self: Arc<Self>, origin: Dest, arg: String) {
        let cwd = if arg.is_empty() {
            None
        } else {
            match self.cfg.project(&arg) {
                Ok((c, _)) => Some(c),
                Err(p) => {
                    return self.emit(Out::Notice {
                        to: origin,
                        text: self.tr.unknown_folder(&p),
                    });
                }
            }
        };
        let params = match &cwd {
            Some(c) => json!({"cwd": c}),
            None => json!({}),
        };
        let list = match self.acp.request("session/list", params).await {
            Ok(v) => v["sessions"].as_array().cloned().unwrap_or_default(),
            Err(e) => {
                return self.emit(Out::Notice {
                    to: origin,
                    text: self.tr.devin_error(&e.to_string()),
                });
            }
        };
        let open: HashSet<String> = self.state.lock_ok().sessions.keys().cloned().collect();
        let mut keyboard = Vec::new();
        for s in list
            .iter()
            .filter(|s| s["sessionId"].as_str().is_some_and(|id| !open.contains(id)))
            .take(RESUME_MAX)
        {
            let token = self.next();
            let scwd = s["cwd"].as_str().unwrap_or(&self.cfg.cwd).to_string();
            let title = s["title"].as_str().unwrap_or("").to_string();
            let date = s["updatedAt"]
                .as_str()
                .unwrap_or("")
                .get(..10)
                .unwrap_or("");
            let locked = s["_meta"]["cognition.ai/isLocked"] == true;
            let label = format!(
                "{}{} · {} · {date}",
                if locked { "🔒 " } else { "" },
                self.cfg.tag_for(&scwd),
                title_of(&title).chars().take(36).collect::<String>()
            );
            self.offers.lock_ok().insert(
                token,
                Offer {
                    session: s["sessionId"].as_str().unwrap_or("").to_string(),
                    cwd: scwd,
                    title,
                    locked_pid: locked
                        .then(|| s["_meta"]["cognition.ai/lockHolderPid"].to_string()),
                },
            );
            keyboard.push(json!([{"text": label, "callback_data": format!("r:{token}")}]));
        }
        if keyboard.is_empty() {
            return self.emit(Out::Notice {
                to: origin,
                text: self.tr.resume_none().to_string(),
            });
        }
        if let Err(e) = self
            .bot
            .send(
                origin,
                self.tr.resume_header(),
                false,
                Some(json!({"inline_keyboard": keyboard})),
            )
            .await
        {
            warn!(error = %e, "resume list not sent");
        }
    }

    async fn on_button(self: &Arc<Self>, cb: &str, m: &Message, data: &str) {
        let answer = |text: String| {
            self.bot.try_call(
                "answerCallbackQuery",
                json!({"callback_query_id": cb, "text": text}),
            )
        };
        if let Some(rest) = data.strip_prefix("p:") {
            let (qid, idx) = rest.split_once(':').unwrap_or((rest, ""));
            let (Ok(qid), Ok(idx)) = (qid.parse::<u64>(), idx.parse::<usize>()) else {
                return;
            };
            let Some(p) = self.perms.lock_ok().remove(&qid) else {
                let known = self.answered.lock_ok().contains(&qid);
                let text = if known {
                    self.tr.already_answered()
                } else {
                    self.tr.expired()
                };
                return answer(text.to_string()).await;
            };
            self.answered.lock_ok().insert(qid);
            let label = p.labels.get(idx).cloned().unwrap_or_default();
            let (chat, msg) = p.msg.unwrap_or((m.chat.id, m.message_id));
            let text = self
                .session(&p.session)
                .map(|s| Self::header(&s, &p.text))
                .unwrap_or(p.text.clone());
            self.bot
                .try_call(
                    "editMessageText",
                    json!({"chat_id": chat, "message_id": msg, "text": format!("{text}\n\n→ {label}")}),
                )
                .await;
            p.tx.send(p.option_ids.get(idx).cloned()).ok();
            answer(format!("✓ {label}")).await;
        } else if let Some(token) = data.strip_prefix("m:") {
            let Some((sid, model)) = token
                .parse::<u64>()
                .ok()
                .and_then(|t| self.model_offers.lock_ok().remove(&t))
            else {
                return answer(self.tr.expired().to_string()).await;
            };
            answer(format!("🧠 {model}")).await;
            let me = self.clone();
            tokio::spawn(async move { me.set_model(&sid, &model).await });
        } else if let Some(token) = data.strip_prefix("r:") {
            let Some(offer) = token
                .parse::<u64>()
                .ok()
                .and_then(|t| self.offers.lock_ok().remove(&t))
            else {
                return answer(self.tr.expired().to_string()).await;
            };
            if let Some(pid) = &offer.locked_pid {
                return answer(self.tr.resume_locked(pid)).await;
            }
            answer(format!("▶️ {}", title_of(&offer.title))).await;
            let origin = Dest {
                chat: m.chat.id,
                thread: m.message_thread_id.filter(|_| m.is_topic_message),
            };
            let me = self.clone();
            tokio::spawn(async move { me.resume(origin, offer).await });
        }
    }

    async fn resume(self: Arc<Self>, origin: Dest, offer: Offer) {
        let title = format!(
            "[{}] {}",
            self.cfg.tag_for(&offer.cwd),
            title_of(&offer.title)
        );
        if let Err(e) = self.load(&offer.session, &offer.cwd).await {
            return self.emit(Out::Notice {
                to: origin,
                text: self.tr.not_resumed(&e.to_string()),
            });
        }
        let dest = self.place(origin, &title).await;
        self.adopt(&offer.session, &title, &offer.cwd, dest);
        if dest != origin
            && let Some(t) = dest.thread
        {
            self.emit(Out::Notice {
                to: origin,
                text: self
                    .tr
                    .new_in_topic(&title, &telegram::topic_link(dest.chat, t)),
            });
        }
        self.emit(Out::Text {
            sid: offer.session,
            text: self.tr.resumed(&title),
            silent: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        assert_eq!(command("/stop"), Some(Command_::Stop));
        assert_eq!(command("/stop@MyDevinBot"), Some(Command_::Stop));
        assert_eq!(command("/mode  plan "), Some(Command_::Mode("plan".into())));
        assert_eq!(
            command("/new app: fix it"),
            Some(Command_::New("app: fix it".into()))
        );
        assert_eq!(command("/cafe off"), Some(Command_::Caffeine("off".into())));
        assert_eq!(command("/lier"), Some(Command_::Link));
        assert_eq!(command("/resume app"), Some(Command_::Resume("app".into())));
        // Devin's own commands are forwarded.
        assert_eq!(command("/compact"), None);
        assert_eq!(command("stop"), None);
        assert_eq!(
            command("/model opus 5.5"),
            Some(Command_::Model("opus 5.5".into()))
        );
        assert_eq!(mode_id("Code"), Some("accept-edits"));
        assert_eq!(mode_id("yolo"), Some("bypass"));
        assert_eq!(mode_id("nope"), None);
    }

    #[test]
    fn models_from_config_and_fuzzy_match() {
        let mut l = Live::default();
        l.apply(&json!({
            "modes": {"currentModeId": "accept-edits"},
            "configOptions": [
                {"id": "mode", "currentValue": "plan"},
                {"id": "model", "currentValue": "swe-2-high", "options": [
                    {"value": "swe-2-high", "name": "SWE-2"},
                    {"value": "claude-opus-5-5-medium", "name": "Claude Opus 5.5"},
                    {"value": "claude-opus-4-8-medium", "name": "Claude Opus 4.8"},
                    {"value": "claude-sonnet-5-5-medium", "name": "Claude Sonnet 5.5"},
                    {"value": "claude-sonnet-5-medium", "name": "Claude Sonnet 5"},
                    {"value": "fusion-claude-sonnet-5-5-high-sidekick-swe-2-medium",
                     "name": "Fusion (Claude Sonnet 5.5 High + SWE-2 Medium)"},
                    {"value": "fusion-gpt-6-sol-high-sidekick-claude-sonnet-5-5-medium",
                     "name": "Fusion (GPT-6 Sol High Thinking + Claude Sonnet 5.5 Medium)"},
                    {"value": "gpt-6-sol-medium", "name": "GPT-6 Sol"}]}]
        }));
        assert_eq!((l.mode.as_str(), l.model.as_str()), ("plan", "swe-2-high"));
        assert_eq!(l.model_name(), "SWE-2");
        l.apply(
            &json!({"configOptions": [{"id": "thought_level", "currentValue": "max",
            "options": [{"value": "medium"}, {"value": "high"}, {"value": "max"}]}]}),
        );
        assert_eq!(l.model_name(), "SWE-2 Max");
        assert_eq!(l.efforts, ["medium", "high", "max"]);
        assert_eq!(
            command("/effort high"),
            Some(Command_::Effort("high".into()))
        );
        let ids = |q: &str| -> Vec<String> {
            match_models(&l.models, q)
                .into_iter()
                .map(|(id, _)| id)
                .collect()
        };
        assert_eq!(ids("opus 5.5"), ["claude-opus-5-5-medium"]);
        assert_eq!(ids("sonnet 5.5"), ["claude-sonnet-5-5-medium"]);
        assert_eq!(
            ids("Opus"),
            ["claude-opus-5-5-medium", "claude-opus-4-8-medium"]
        );
        assert_eq!(ids("swe-2"), ["swe-2-high"]);
        assert_eq!(ids("GPT-6 Sol"), ["gpt-6-sol-medium"]);
        assert_eq!(
            ids("5.5"),
            ["claude-opus-5-5-medium", "claude-sonnet-5-5-medium"]
        );
        // Word fallback: every word, in any order.
        assert_eq!(
            ids("sol gpt"),
            [
                "fusion-gpt-6-sol-high-sidekick-claude-sonnet-5-5-medium",
                "gpt-6-sol-medium"
            ]
        );
        assert!(ids("llama").is_empty());
        assert!(ids("  ").is_empty());
    }

    #[test]
    fn titles_and_labels() {
        assert_eq!(title_of("fix the test\nthen the PR"), "fix the test");
        assert_eq!(title_of(&"x".repeat(60)).chars().count(), 49);
        let o = json!({"optionId": "allow_once", "name": "Allow", "kind": "allow_once"});
        assert_eq!(option_label(&o), "✅ Allow");
        let o = json!({"optionId": "r", "name": "No", "kind": "reject_once"});
        assert_eq!(option_label(&o), "⛔ No");
    }
}
