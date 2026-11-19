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
//! ```
//!
//! `{id}` is replaced by the session id and `{cwd}` by the session folder,
//! both shell-quoted. The command runs through the shell in the session
//! folder, so `~` and environment variables work.

use std::path::PathBuf;

pub struct Config {
    pub open: String,
    pub open_label: String,
    pub alt_open: Option<String>,
    pub alt_label: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            open: "claude --resume {id}".into(),
            open_label: "open".into(),
            alt_open: None,
            alt_label: "alt".into(),
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
                _ => {}
            }
        }
        cfg
    }

    /// The shell command for a session, `alt` selects the Alt-v command.
    pub fn command(&self, alt: bool, session_id: &str, cwd: &str) -> Option<String> {
        let template = if alt {
            self.alt_open.as_deref()?
        } else {
            &self.open
        };
        Some(
            template
                .replace("{id}", &shell_quote(session_id))
                .replace("{cwd}", &shell_quote(cwd)),
        )
    }
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
            d.command(false, "abc", "/w").unwrap(),
            "claude --resume 'abc'"
        );
        assert_eq!(d.command(true, "abc", "/w"), None);

        let c = Config::parse(
            "# comment\nopen = claude --agent a --resume {id}\nopen_label=a\n\
             alt_open = cd {cwd} && claude -r {id}\nalt_label = b\nunknown = x\nbroken line\n",
        );
        assert_eq!(c.open_label, "a");
        assert_eq!(c.alt_label, "b");
        assert_eq!(
            c.command(false, "abc", "/w").unwrap(),
            "claude --agent a --resume 'abc'"
        );
        assert_eq!(
            c.command(true, "abc", "/it's").unwrap(),
            r"cd '/it'\''s' && claude -r 'abc'"
        );
    }
}
