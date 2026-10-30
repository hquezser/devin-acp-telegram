//! Configuration: environment variables, optionally loaded from
//! `~/.config/devin-acp-telegram/config.env` (`KEY=VALUE` lines). Real
//! environment variables win over the file.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};

use crate::i18n::Lang;

#[derive(Debug, Clone)]
pub struct Config {
    /// Bot token from @BotFather (secret, never logged).
    pub token: String,
    /// Telegram user ids allowed to drive the agent.
    pub allowed_users: Vec<i64>,
    /// Default working directory for new sessions.
    pub cwd: String,
    /// Project aliases → directories, in declared order.
    pub projects: Vec<(String, String)>,
    /// Agent command (`devin`), run as `<bin> acp <args…>`.
    pub devin_bin: String,
    pub acp_args: Vec<String>,
    /// Mode applied to new sessions (`accept-edits`, `smart`, `plan`…).
    pub mode: Option<String>,
    /// Keep the Mac awake while running (`caffeinate`).
    pub caffeine: bool,
    pub lang: Lang,
    /// Bot API base URL (tests point it at a fake server).
    pub api_url: String,
    /// Persistent state (sessions, linked group) and lock file.
    pub state_dir: PathBuf,
}

/// Default config file location.
pub fn config_file() -> Option<PathBuf> {
    std::env::var_os("DEVIN_TG_CONFIG")
        .map(PathBuf::from)
        .or_else(|| home().map(|h| h.join(".config/devin-acp-telegram/config.env")))
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Parses `KEY=VALUE` lines (comments, blank lines, optional quotes).
pub fn parse_env_file(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.is_empty() || l.starts_with('#') {
                return None;
            }
            let l = l.strip_prefix("export ").unwrap_or(l);
            let (k, v) = l.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_string(), v.to_string()))
        })
        .collect()
}

/// `~/x` → `$HOME/x`.
pub fn expand(path: &str) -> String {
    match (path.strip_prefix("~/"), home()) {
        (Some(rest), Some(h)) => h.join(rest).display().to_string(),
        _ if path == "~" => home().map_or(path.to_string(), |h| h.display().to_string()),
        _ => path.to_string(),
    }
}

/// `app=~/code/app,api=/srv/api` → pairs (aliases lower-cased).
pub fn parse_projects(spec: &str) -> Vec<(String, String)> {
    spec.split(',')
        .filter_map(|p| {
            let (alias, path) = p.split_once('=')?;
            let (alias, path) = (alias.trim().to_lowercase(), path.trim());
            (!alias.is_empty() && !path.is_empty()).then(|| (alias, expand(path)))
        })
        .collect()
}

impl Config {
    /// Environment first, then the config file. `cwd` = CLI directory
    /// argument, else `DEVIN_CWD`, else the current directory.
    pub fn load(cli_dir: Option<&str>) -> anyhow::Result<Config> {
        let mut vars: HashMap<String, String> = HashMap::new();
        if let Some(f) = config_file()
            && let Ok(text) = std::fs::read_to_string(&f)
        {
            vars.extend(parse_env_file(&text));
        }
        vars.extend(std::env::vars());
        let cwd = match cli_dir {
            Some(d) => d.to_string(),
            None => match vars.get("DEVIN_CWD").filter(|v| !v.trim().is_empty()) {
                Some(d) => expand(d),
                None => std::env::current_dir()?.display().to_string(),
            },
        };
        Config::from_map(&vars, cwd)
    }

    pub fn from_map(vars: &HashMap<String, String>, cwd: String) -> anyhow::Result<Config> {
        let var = |k: &str| {
            vars.get(k)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let token = var("TELEGRAM_BOT_TOKEN")
            .context("TELEGRAM_BOT_TOKEN is missing (create a bot with @BotFather)")?;
        let allowed_users = var("TELEGRAM_ALLOWED_USERS")
            .context(
                "TELEGRAM_ALLOWED_USERS is missing (your Telegram user id, e.g. from @userinfobot)",
            )?
            .split(',')
            .map(|s| s.trim().parse::<i64>())
            .collect::<Result<Vec<_>, _>>()
            .context("TELEGRAM_ALLOWED_USERS must be comma-separated numeric user ids")?;
        if allowed_users.is_empty() {
            bail!("TELEGRAM_ALLOWED_USERS is empty");
        }
        let cwd = std::fs::canonicalize(expand(&cwd))
            .with_context(|| format!("working directory not found: {cwd}"))?
            .display()
            .to_string();
        let state_dir = var("DEVIN_TG_STATE_DIR")
            .map(|d| PathBuf::from(expand(&d)))
            .or_else(|| {
                var("XDG_STATE_HOME")
                    .map(|d| Path::new(&d).join("devin-acp-telegram"))
                    .or_else(|| home().map(|h| h.join(".local/state/devin-acp-telegram")))
            })
            .context("cannot locate a state directory (set DEVIN_TG_STATE_DIR)")?;
        Ok(Config {
            token,
            allowed_users,
            cwd,
            projects: var("DEVIN_PROJECTS")
                .map(|p| parse_projects(&p))
                .unwrap_or_default(),
            devin_bin: var("DEVIN_BIN").unwrap_or_else(|| "devin".to_string()),
            acp_args: var("DEVIN_ACP_ARGS")
                .map(|a| a.split_whitespace().map(str::to_string).collect())
                .unwrap_or_default(),
            mode: var("DEVIN_MODE"),
            caffeine: var("DEVIN_CAFFEINE").is_none_or(|v| v != "0"),
            lang: var("DEVIN_TG_LANG")
                .map(|l| Lang::parse(&l))
                .unwrap_or_default(),
            api_url: var("TELEGRAM_API_URL")
                .unwrap_or_else(|| "https://api.telegram.org".to_string())
                .trim_end_matches('/')
                .to_string(),
            state_dir,
        })
    }

    /// Label of a directory: its alias, else its last path component.
    pub fn tag_for(&self, cwd: &str) -> String {
        let cwd = cwd.trim_end_matches('/');
        self.projects
            .iter()
            .find(|(_, p)| p.trim_end_matches('/') == cwd)
            .map(|(a, _)| a.clone())
            .unwrap_or_else(|| cwd.rsplit('/').next().unwrap_or(cwd).to_string())
    }

    /// Directory of an alias (case-insensitive).
    pub fn alias(&self, name: &str) -> Option<&str> {
        self.projects
            .iter()
            .find(|(a, _)| a.eq_ignore_ascii_case(name))
            .map(|(_, p)| p.as_str())
    }

    /// Project prefix of a new-session prompt: `alias: prompt`, a bare
    /// `alias`, or a path (`~/x: prompt`). Returns (directory, prompt);
    /// anything else (`fix: the bug`) is a whole prompt in the default
    /// directory.
    pub fn project(&self, text: &str) -> Result<(String, String), String> {
        let text = text.trim();
        let (head, rest) = match text.split_once(':') {
            Some((h, r)) if !h.trim().contains(char::is_whitespace) => (h.trim(), r.trim()),
            _ if !text.contains(char::is_whitespace) => (text, ""),
            _ => return Ok((self.cwd.clone(), text.to_string())),
        };
        if let Some(path) = self.alias(head) {
            return Ok((path.to_string(), rest.to_string()));
        }
        if head.starts_with('~') || head.starts_with('/') {
            let path = expand(head);
            return if Path::new(&path).is_dir() {
                Ok((path, rest.to_string()))
            } else {
                Err(head.to_string())
            };
        }
        Ok((self.cwd.clone(), text.to_string()))
    }

    pub fn is_allowed(&self, user: i64) -> bool {
        self.allowed_users.contains(&user)
    }
}

#[cfg(test)]
pub fn test_config(api_url: &str, state_dir: PathBuf) -> Config {
    Config {
        token: "TEST-TOKEN".into(),
        allowed_users: vec![42],
        cwd: "/srv/default".into(),
        projects: vec![("app".into(), "/srv/app".into())],
        devin_bin: "devin".into(),
        acp_args: vec![],
        mode: None,
        caffeine: false,
        lang: Lang::En,
        api_url: api_url.to_string(),
        state_dir,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_file_parsing() {
        let m = parse_env_file(
            "# comment\n\nTELEGRAM_BOT_TOKEN=\"1:abc\"\nexport DEVIN_MODE=plan\nX = 'y z'\nbroken\n",
        );
        assert_eq!(m["TELEGRAM_BOT_TOKEN"], "1:abc");
        assert_eq!(m["DEVIN_MODE"], "plan");
        assert_eq!(m["X"], "y z");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn required_vars_and_defaults() {
        let tmp = std::env::temp_dir();
        let cwd = tmp.display().to_string();
        let mut v = HashMap::new();
        let e = Config::from_map(&v, cwd.clone()).unwrap_err();
        assert!(e.to_string().contains("TELEGRAM_BOT_TOKEN"), "{e}");
        v.insert("TELEGRAM_BOT_TOKEN".into(), "1:abc".into());
        let e = Config::from_map(&v, cwd.clone()).unwrap_err();
        assert!(e.to_string().contains("TELEGRAM_ALLOWED_USERS"), "{e}");
        v.insert("TELEGRAM_ALLOWED_USERS".into(), "12, x".into());
        assert!(Config::from_map(&v, cwd.clone()).is_err());
        v.insert("TELEGRAM_ALLOWED_USERS".into(), "12, 34".into());
        v.insert("DEVIN_TG_STATE_DIR".into(), "/tmp/state".into());
        let c = Config::from_map(&v, cwd.clone()).unwrap();
        assert_eq!(c.allowed_users, [12, 34]);
        assert!(c.caffeine);
        assert_eq!(c.lang, Lang::En);
        assert_eq!(c.devin_bin, "devin");
        assert!(c.is_allowed(34) && !c.is_allowed(56));
        assert!(Config::from_map(&v, "/does/not/exist".into()).is_err());
    }

    #[test]
    fn projects_pick_the_session_folder() {
        let home = std::env::var("HOME").unwrap();
        let mut cfg = test_config("http://x", "/tmp".into());
        cfg.projects = parse_projects(" App = ~/code/app , api=/srv/api,bad");
        assert_eq!(
            cfg.projects,
            [
                ("app".to_string(), format!("{home}/code/app")),
                ("api".to_string(), "/srv/api".to_string()),
            ]
        );
        let p = |t: &str| cfg.project(t).unwrap();
        assert_eq!(
            p("app: fix the test"),
            (format!("{home}/code/app"), "fix the test".into())
        );
        assert_eq!(p("API : run CI"), ("/srv/api".into(), "run CI".into()));
        assert_eq!(p("api"), ("/srv/api".into(), String::new()));
        // Not an alias: the whole prompt, default directory.
        assert_eq!(
            p("fix: the bug"),
            ("/srv/default".into(), "fix: the bug".into())
        );
        assert_eq!(
            p("hello there"),
            ("/srv/default".into(), "hello there".into())
        );
        assert_eq!(
            p("Note : read"),
            ("/srv/default".into(), "Note : read".into())
        );
        // Explicit path.
        assert_eq!(p("/tmp: list"), ("/tmp".into(), "list".into()));
        assert_eq!(cfg.project("/nope/nope: x"), Err("/nope/nope".into()));
        assert_eq!(cfg.tag_for("/srv/api/"), "api");
        assert_eq!(cfg.tag_for("/srv/default"), "default");
    }
}
