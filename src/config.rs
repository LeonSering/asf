//! Optional config file: `~/.config/asf/config`.
//!
//! One `key = value` per line, `#` starts a comment line.
//!
//! ```text
//! # Enter / Alt-a. Default: claude --resume {id}
//! open = claude --resume {id}
//! open_label = open
//!
//! # Alt-v, disabled unless set
//! alt_open = claude --resume {id} --model opus
//! alt_label = opus
//!
//! # Enter on a session running in the background (`claude --bg`)
//! attach = claude attach {short_id}
//!
//! # Alt-s: stop a session running in the background
//! stop = claude stop {short_id}
//! ```
//!
//! `{id}` is replaced by the session id, `{short_id}` by its first 8
//! characters (what `claude attach` expects) and `{cwd}` by the session
//! folder, all shell-quoted. The command runs through the shell in the session
//! folder, so `~` and environment variables work.

use std::path::PathBuf;

pub struct Config {
    pub open: String,
    pub open_label: String,
    pub alt_open: Option<String>,
    pub alt_label: String,
    pub attach: String,
    pub stop: String,
}

/// Which configured command to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Which {
    Open,
    Alt,
    Attach,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            open: "claude --resume {id}".into(),
            open_label: "open".into(),
            alt_open: None,
            alt_label: "alt".into(),
            attach: "claude attach {short_id}".into(),
            stop: "claude stop {short_id}".into(),
        }
    }
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("asf/config"))
}

impl Config {
    /// Load the config file. A missing file or missing keys use the defaults.
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    fn parse(text: &str) -> Self {
        let mut cfg = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().to_string();
            if value.is_empty() {
                continue;
            }
            match key.trim() {
                "open" => cfg.open = value,
                "open_label" => cfg.open_label = value,
                "alt_open" => cfg.alt_open = Some(value),
                "alt_label" => cfg.alt_label = value,
                "attach" => cfg.attach = value,
                "stop" => cfg.stop = value,
                _ => {}
            }
        }
        cfg
    }

    /// The shell command for a session. None if Alt is not configured.
    pub fn command(&self, which: Which, session_id: &str, cwd: &str) -> Option<String> {
        let template = match which {
            Which::Open => &self.open,
            Which::Alt => self.alt_open.as_deref()?,
            Which::Attach => &self.attach,
        };
        Some(render(template, session_id, cwd))
    }
}

/// Fill in `{id}`, `{short_id}` and `{cwd}` (shell-quoted).
pub fn render(template: &str, session_id: &str, cwd: &str) -> String {
    let short: String = session_id.chars().take(8).collect();
    template
        .replace("{short_id}", &shell_quote(&short))
        .replace("{id}", &shell_quote(session_id))
        .replace("{cwd}", &shell_quote(cwd))
}

pub fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_overrides() {
        let d = Config::default();
        assert_eq!(
            d.command(Which::Open, "abc", "/w").unwrap(),
            "claude --resume 'abc'"
        );
        assert_eq!(d.command(Which::Alt, "abc", "/w"), None);

        let c = Config::parse(
            "# comment\nopen = claude --agent a --resume {id}\nopen_label=a\n\
             alt_open = cd {cwd} && claude -r {id}\nalt_label = b\nunknown = x\nbroken line\n",
        );
        assert_eq!(
            d.command(Which::Attach, "5f908249-5d86-48e3", "/w")
                .unwrap(),
            "claude attach '5f908249'"
        );
        assert_eq!(c.open_label, "a");
        assert_eq!(c.alt_label, "b");
        assert_eq!(
            c.command(Which::Open, "abc", "/w").unwrap(),
            "claude --agent a --resume 'abc'"
        );
        assert_eq!(
            c.command(Which::Alt, "abc", "/it's").unwrap(),
            r"cd '/it'\''s' && claude -r 'abc'"
        );
    }
}
