//! Persistent state (`state.json` in the state directory): open sessions
//! and where they live in Telegram, the linked forum group, and which
//! Telegram message belongs to which session (so replies route back).

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::telegram::Dest;

/// Messages remembered for reply routing.
const MSG_INDEX_CAP: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Session {
    pub title: String,
    pub cwd: String,
    pub chat: i64,
    pub thread: Option<i64>,
    /// Status message edited in place during the current turn.
    pub status_msg: Option<i64>,
    /// Unix seconds of the last activity (most recent = default target).
    pub updated: u64,
}

impl Session {
    pub fn dest(&self) -> Dest {
        Dest {
            chat: self.chat,
            thread: self.thread,
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    /// Forum group linked with /link.
    pub group: Option<i64>,
    /// Next Telegram update to fetch (survives restarts: no command is
    /// processed twice).
    #[serde(default)]
    pub offset: i64,
    pub sessions: BTreeMap<String, Session>,
    /// (chat, message id, session id).
    msg_index: VecDeque<(i64, i64, String)>,
    #[serde(skip)]
    path: PathBuf,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl State {
    pub fn load(dir: &Path) -> State {
        let path = dir.join("state.json");
        let mut s: State = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        s.path = path;
        s
    }

    /// Atomic write (temp file + rename); failures are logged only.
    pub fn save(&self) {
        let tmp = self.path.with_extension("json.tmp");
        let r = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, &self.path));
        if let Err(e) = r {
            warn!(error = %e, "state not saved");
        }
    }

    pub fn index_msg(&mut self, chat: i64, msg: i64, session: &str) {
        self.msg_index.push_back((chat, msg, session.to_string()));
        while self.msg_index.len() > MSG_INDEX_CAP {
            self.msg_index.pop_front();
        }
    }

    pub fn session_of_msg(&self, chat: i64, msg: i64) -> Option<String> {
        self.msg_index
            .iter()
            .rev()
            .find(|(c, m, _)| *c == chat && *m == msg)
            .map(|(_, _, s)| s.clone())
    }

    pub fn session_of_thread(&self, chat: i64, thread: i64) -> Option<String> {
        self.sessions
            .iter()
            .find(|(_, s)| s.chat == chat && s.thread == Some(thread))
            .map(|(id, _)| id.clone())
    }

    /// Most recently active session.
    pub fn latest(&self) -> Option<String> {
        self.sessions
            .iter()
            .max_by_key(|(_, s)| s.updated)
            .map(|(id, _)| id.clone())
    }

    pub fn touch(&mut self, id: &str) {
        if let Some(s) = self.sessions.get_mut(id) {
            s.updated = now();
        }
    }

    pub fn remove(&mut self, id: &str) {
        self.sessions.remove(id);
        self.msg_index.retain(|(_, _, s)| s != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(chat: i64, thread: Option<i64>, updated: u64) -> Session {
        Session {
            title: "t".into(),
            cwd: "/c".into(),
            chat,
            thread,
            status_msg: None,
            updated,
        }
    }

    #[test]
    fn round_trip_and_lookups() {
        let dir = std::env::temp_dir().join(format!("dat-state-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = State::load(&dir);
        assert!(s.sessions.is_empty() && s.group.is_none());
        s.group = Some(-100);
        s.sessions.insert("a".into(), session(-100, Some(7), 10));
        s.sessions.insert("b".into(), session(42, None, 20));
        s.index_msg(42, 5, "b");
        s.save();
        let mut s = State::load(&dir);
        assert_eq!(s.group, Some(-100));
        assert_eq!(s.session_of_thread(-100, 7).as_deref(), Some("a"));
        assert_eq!(s.session_of_thread(-100, 8), None);
        assert_eq!(s.session_of_msg(42, 5).as_deref(), Some("b"));
        assert_eq!(s.latest().as_deref(), Some("b"));
        s.touch("a");
        assert_eq!(s.latest().as_deref(), Some("a"));
        s.remove("b");
        assert_eq!(s.session_of_msg(42, 5), None);
        for i in 0..MSG_INDEX_CAP + 10 {
            s.index_msg(1, i as i64, "a");
        }
        assert_eq!(s.session_of_msg(1, 0), None);
        assert!(s.session_of_msg(1, (MSG_INDEX_CAP + 9) as i64).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }
}
