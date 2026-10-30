//! User-facing strings (English default, French via `DEVIN_TG_LANG=fr`).

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lang {
    #[default]
    En,
    Fr,
}

impl Lang {
    pub fn parse(s: &str) -> Lang {
        if s.trim().to_lowercase().starts_with("fr") {
            Lang::Fr
        } else {
            Lang::En
        }
    }
}

/// Translations, picked by `Lang`.
#[derive(Debug, Clone, Copy)]
pub struct Tr(pub Lang);

macro_rules! tr {
    ($self:ident, $en:expr, $fr:expr) => {
        match $self.0 {
            Lang::En => $en,
            Lang::Fr => $fr,
        }
    };
}

impl Tr {
    pub fn online(&self, host: &str, cwd: &str, caffeine: bool) -> String {
        let cafe = if caffeine { " · ☕" } else { "" };
        tr!(
            self,
            format!("🔌 Devin bridge online · {host} · {cwd}{cafe}"),
            format!("🔌 Pont Devin en ligne · {host} · {cwd}{cafe}")
        )
    }

    pub fn offline(&self, error: Option<&str>) -> String {
        match error {
            None => tr!(self, "🔌 Devin bridge stopped.", "🔌 Pont Devin arrêté.").to_string(),
            Some(e) => tr!(
                self,
                format!("🔴 Devin bridge stopped: {e}"),
                format!("🔴 Pont Devin arrêté : {e}")
            ),
        }
    }

    pub fn help(&self) -> &'static str {
        tr!(
            self,
            "Devin CLI remote control\n\
• just write: instruction for the current session (or a new one)\n\
• reply to a session message: instruction for that session\n\
• /new [project:] <text>: new session (/projects for aliases)\n\
• /resume [project]: pick a recent Devin session to continue\n\
• /stop: interrupt the current turn\n\
• /mode code|smart|ask|plan|bypass\n\
• /sessions: bridge and session status\n\
• /close: end the session\n\
• /caffeine on|off: keep the Mac awake (or not)\n\
• /link (in a group with Topics): one topic per session; /unlink to undo\n\
Other commands (/compact, /plan…) go to Devin.",
            "Pilotage de Devin CLI\n\
• écris simplement : consigne à la session en cours (ou nouvelle session)\n\
• réponds à un message d'une session : consigne à cette session\n\
• /new [projet:] <texte> : nouvelle session (/projects pour les alias)\n\
• /resume [projet] : reprendre une session Devin récente\n\
• /stop : interrompt le tour en cours\n\
• /mode code|smart|ask|plan|bypass\n\
• /sessions : état du pont et des sessions\n\
• /close : termine la session\n\
• /caffeine on|off : empêche (ou non) la veille du Mac\n\
• /link (dans un groupe à Sujets) : un sujet par session ; /unlink pour revenir\n\
Les autres commandes (/compact, /plan…) partent à Devin."
        )
    }

    pub fn no_session(&self) -> &'static str {
        tr!(
            self,
            "No open Devin session.",
            "Aucune session Devin ouverte."
        )
    }

    pub fn stop_requested(&self, title: &str) -> String {
        tr!(
            self,
            format!("⏹ Stop requested: {title}"),
            format!("⏹ Arrêt demandé : {title}")
        )
    }

    pub fn closed(&self) -> &'static str {
        tr!(self, "Session closed.", "Session terminée.")
    }

    pub fn modes_usage(&self) -> &'static str {
        tr!(
            self,
            "Modes: code, smart, ask, plan, bypass.",
            "Modes : code, smart, ask, plan, bypass."
        )
    }

    pub fn mode_set(&self, mode: &str) -> String {
        tr!(
            self,
            format!("⚙️ Mode: {mode}"),
            format!("⚙️ Mode : {mode}")
        )
    }

    pub fn mode_refused(&self, mode: &str, e: &str) -> String {
        tr!(
            self,
            format!("🔴 Mode {mode} refused: {e}"),
            format!("🔴 Mode {mode} refusé : {e}")
        )
    }

    pub fn new_usage(&self) -> &'static str {
        tr!(
            self,
            "Usage: /new [project:] <instruction>",
            "Usage : /new [projet:] <consigne>"
        )
    }

    pub fn caffeine(&self, on: bool) -> &'static str {
        match (self.0, on) {
            (Lang::En, true) => {
                "☕ Caffeine on: the Mac stays awake (lid open, or on power with an external display)."
            }
            (Lang::En, false) => "Caffeine off: the Mac may sleep.",
            (Lang::Fr, true) => {
                "☕ Caféine on : le Mac reste éveillé (capot ouvert, ou sur secteur avec écran externe)."
            }
            (Lang::Fr, false) => "Caféine off : le Mac peut se mettre en veille.",
        }
    }

    pub fn caffeine_usage(&self) -> &'static str {
        tr!(self, "Usage: /caffeine on|off", "Usage : /caffeine on|off")
    }

    pub fn linked(&self, forum: bool) -> &'static str {
        match (self.0, forum) {
            (Lang::En, true) => {
                "✅ Group linked: one topic per Devin session. Write in General to start a session; /help for commands. The bot must be an admin allowed to manage topics."
            }
            (Lang::En, false) => {
                "✅ Group linked, but Topics are off: enable them in the group settings and let the bot manage topics. Until then sessions stay in the private chat."
            }
            (Lang::Fr, true) => {
                "✅ Groupe lié : un sujet par session Devin. Écris dans « Général » pour démarrer une session ; /help pour les commandes. Le bot doit être admin avec le droit « Gérer les sujets »."
            }
            (Lang::Fr, false) => {
                "✅ Groupe lié, mais les Sujets ne sont pas activés : active-les dans les paramètres du groupe et donne au bot le droit « Gérer les sujets ». En attendant, les sessions restent dans le chat privé."
            }
        }
    }

    pub fn unlinked(&self) -> &'static str {
        tr!(
            self,
            "Group unlinked: new sessions go to the private chat.",
            "Groupe délié : les nouvelles sessions passent par le chat privé."
        )
    }

    pub fn unlink_where(&self) -> &'static str {
        tr!(
            self,
            "Send /unlink in the linked group.",
            "À envoyer dans le groupe lié."
        )
    }

    pub fn bridge_line(&self, host: &str, caffeine: bool, open: usize) -> String {
        let cafe = if caffeine { "☕ on" } else { "off" };
        tr!(
            self,
            format!("🔌 {host} · caffeine {cafe} · {open} open session(s)"),
            format!("🔌 {host} · caféine {cafe} · {open} session(s) ouverte(s)")
        )
    }

    pub fn projects(&self, default: &str, list: &[(String, String)]) -> String {
        let mut out = tr!(
            self,
            format!("📁 Default: {default}"),
            format!("📁 Par défaut : {default}")
        );
        if list.is_empty() {
            out.push_str(tr!(
                self,
                "\nNo aliases (set DEVIN_PROJECTS).",
                "\nAucun alias (DEVIN_PROJECTS)."
            ));
        }
        for (a, p) in list {
            out.push_str(&format!("\n• {a} → {p}"));
        }
        out.push_str(tr!(
            self,
            "\n\n/new <project>: <instruction> (or a path ~/…: <instruction>)",
            "\n\n/new <projet>: <consigne> (ou un chemin ~/…: <consigne>)"
        ));
        out
    }

    pub fn routed(&self, title: &str) -> String {
        format!("→ {title}")
    }

    pub fn new_in_topic(&self, title: &str, link: &str) -> String {
        tr!(
            self,
            format!("🆕 {title}\n{link}"),
            format!("🆕 {title}\n{link}")
        )
    }

    pub fn opened_empty(&self, cwd: &str) -> String {
        tr!(
            self,
            format!("Session opened in {cwd}. Waiting for your instruction."),
            format!("Session ouverte dans {cwd}. J'attends ta consigne.")
        )
    }

    pub fn new_failed(&self, e: &str) -> String {
        tr!(
            self,
            format!("🔴 Cannot start a Devin session: {e}"),
            format!("🔴 Nouvelle session Devin impossible : {e}")
        )
    }

    pub fn unknown_folder(&self, path: &str) -> String {
        tr!(
            self,
            format!("🔴 Folder not found: {path}"),
            format!("🔴 Dossier introuvable : {path}")
        )
    }

    pub fn not_resumed(&self, e: &str) -> String {
        tr!(
            self,
            format!("🔴 Devin could not resume this session: {e}"),
            format!("🔴 Devin n'a pas pu reprendre cette session : {e}")
        )
    }

    pub fn interrupted(&self) -> &'static str {
        tr!(self, "⏹ Interrupted.", "⏹ Interrompu.")
    }

    pub fn turn_end(&self, reason: &str) -> String {
        tr!(
            self,
            format!("(turn ended: {reason})"),
            format!("(fin du tour : {reason})")
        )
    }

    pub fn devin_error(&self, e: &str) -> String {
        tr!(
            self,
            format!("🔴 Devin error: {e}"),
            format!("🔴 Erreur Devin : {e}")
        )
    }

    pub fn st_working(&self) -> &'static str {
        tr!(self, "🟢 Working", "🟢 Au travail")
    }

    pub fn st_waiting(&self, what: &str) -> String {
        tr!(
            self,
            format!("🟡 Waiting for your approval: {what}"),
            format!("🟡 Attend ton accord : {what}")
        )
    }

    pub fn st_idle(&self) -> &'static str {
        tr!(self, "⚪ Your turn", "⚪ À toi")
    }

    pub fn st_stopped(&self) -> &'static str {
        tr!(self, "⏹ Interrupted · your turn", "⏹ Interrompu · à toi")
    }

    pub fn st_failed(&self, tool: &str) -> String {
        tr!(
            self,
            format!("⚠️ failed: {tool}"),
            format!("⚠️ échec : {tool}")
        )
    }

    pub fn st_context(&self, used_k: u64, size_k: u64) -> String {
        tr!(
            self,
            format!("context {used_k}k/{size_k}k"),
            format!("contexte {used_k}k/{size_k}k")
        )
    }

    pub fn permission(&self, title: &str) -> String {
        tr!(
            self,
            format!("🔐 Allow? {title}"),
            format!("🔐 Autoriser ? {title}")
        )
    }

    pub fn already_answered(&self) -> &'static str {
        tr!(self, "Already answered.", "Déjà répondu.")
    }

    pub fn expired(&self) -> &'static str {
        tr!(
            self,
            "Expired (the bridge restarted).",
            "Expiré (le pont a redémarré)."
        )
    }

    pub fn resume_header(&self) -> &'static str {
        tr!(
            self,
            "Recent Devin sessions: tap one to continue it here.",
            "Sessions Devin récentes : touche celle à reprendre ici."
        )
    }

    pub fn resume_none(&self) -> &'static str {
        tr!(
            self,
            "No Devin session to resume.",
            "Aucune session Devin à reprendre."
        )
    }

    pub fn resume_locked(&self, pid: &str) -> String {
        tr!(
            self,
            format!("⚠️ This session is open in a terminal (pid {pid}): close it there first."),
            format!(
                "⚠️ Cette session est ouverte dans un terminal (pid {pid}) : ferme-la d'abord."
            )
        )
    }

    pub fn resumed(&self, title: &str) -> String {
        tr!(
            self,
            format!("▶️ Resumed: {title}. Send your next instruction."),
            format!("▶️ Reprise : {title}. Envoie ta prochaine consigne.")
        )
    }
}
