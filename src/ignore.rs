//! Ignore file: `~/.config/asf/ignore`, one pattern per line, gitignore style.
//!
//! - A plain line is a folder pattern, matched against the session's working
//!   directory. Ignoring a folder also ignores everything below it.
//!   `~` is the home directory, `*` and `?` stay within one path component,
//!   `**` spans any number of components. A pattern without `/` (e.g.
//!   `scratch`) matches a folder of that name at any depth.
//! - `id:<session id>` ignores one session, `name:<glob>` matches session names.
//! - `!` in front negates a pattern. The last matching line decides.
//! - Empty lines and lines starting with `#` are skipped.

use crate::scanner::SessionMeta;
use std::path::{Path, PathBuf};

enum Kind {
    /// Path components of the folder pattern
    Folder(Vec<String>),
    Id(String),
    Name(String),
}

struct Rule {
    negate: bool,
    kind: Kind,
}

#[derive(Default)]
pub struct IgnoreList {
    rules: Vec<Rule>,
}

pub fn ignore_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("asf/ignore"))
}

impl IgnoreList {
    /// Load the ignore file. A missing or unreadable file ignores nothing.
    pub fn load() -> Self {
        ignore_file_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|text| Self::parse(&text, dirs::home_dir().as_deref()))
            .unwrap_or_default()
    }

    fn parse(text: &str, home: Option<&Path>) -> Self {
        let mut rules = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negate, line) = match line.strip_prefix('!') {
                Some(rest) => (true, rest.trim_start()),
                None => (false, line),
            };
            let kind = if let Some(id) = line.strip_prefix("id:") {
                Kind::Id(id.trim().to_string())
            } else if let Some(name) = line.strip_prefix("name:") {
                Kind::Name(name.trim().to_string())
            } else {
                let mut pat = line.trim_end_matches('/').to_string();
                if pat == "~" || pat.starts_with("~/") {
                    match home {
                        Some(h) => pat = format!("{}{}", h.to_string_lossy(), &pat[1..]),
                        None => continue,
                    }
                }
                if !pat.contains('/') {
                    // Bare name: any folder of that name.
                    pat = format!("**/{pat}");
                }
                Kind::Folder(components(&pat))
            };
            rules.push(Rule { negate, kind });
        }
        Self { rules }
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    pub fn is_ignored(&self, s: &SessionMeta) -> bool {
        let cwd = components(&s.cwd);
        let mut ignored = false;
        for rule in &self.rules {
            let hit = match &rule.kind {
                Kind::Id(id) => *id == s.session_id,
                Kind::Name(glob) => s
                    .session_name
                    .as_deref()
                    .is_some_and(|n| match_segment(glob.as_bytes(), n.as_bytes())),
                // The folder itself or any of its parents.
                Kind::Folder(pat) => (1..=cwd.len()).any(|n| match_path(pat, &cwd[..n])),
            };
            if hit {
                ignored = !rule.negate;
            }
        }
        ignored
    }
}

/// Path components; a leading `/` is kept as an empty first component so
/// absolute patterns only match absolute paths from the root.
fn components(p: &str) -> Vec<String> {
    let mut out: Vec<String> = p
        .split('/')
        .filter(|c| !c.is_empty())
        .map(str::to_string)
        .collect();
    if p.starts_with('/') {
        out.insert(0, String::new());
    }
    out
}

fn match_path(pat: &[String], path: &[String]) -> bool {
    match pat.split_first() {
        None => path.is_empty(),
        Some((first, rest)) if first == "**" => {
            (0..=path.len()).any(|skip| match_path(rest, &path[skip..]))
        }
        Some((first, rest)) => match path.split_first() {
            Some((p, prest)) => {
                match_segment(first.as_bytes(), p.as_bytes()) && match_path(rest, prest)
            }
            None => false,
        },
    }
}

/// `*` and `?` glob matching within one component (byte-wise; `?` matches
/// one byte, which is fine for ASCII folder names).
fn match_segment(pat: &[u8], text: &[u8]) -> bool {
    match pat.split_first() {
        None => text.is_empty(),
        Some((b'*', rest)) => (0..=text.len()).any(|i| match_segment(rest, &text[i..])),
        Some((b'?', rest)) => !text.is_empty() && match_segment(rest, &text[1..]),
        Some((c, rest)) => text.first() == Some(c) && match_segment(rest, &text[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(cwd: &str, id: &str, name: Option<&str>) -> SessionMeta {
        SessionMeta {
            session_id: id.into(),
            file_path: String::new(),
            cwd: cwd.into(),
            session_name: name.map(str::to_string),
            first_user_message: None,
            last_modified: 0,
            git_branch: None,
            file_mtime: 0,
            file_len: 0,
        }
    }

    #[test]
    fn folder_patterns() {
        let home = Path::new("/home/u");
        let list = IgnoreList::parse(
            "# comment\n\n~/tmp/\n/var/*/cache\nscratch\n~/work/*/old\n!~/tmp/keep\n",
            Some(home),
        );
        let ign = |cwd: &str| list.is_ignored(&session(cwd, "x", None));
        assert!(ign("/home/u/tmp"));
        assert!(ign("/home/u/tmp/a/b"), "subfolders too");
        assert!(!ign("/home/u/tmpx"));
        assert!(!ign("/home/u/tmp/keep"), "negated");
        assert!(!ign("/home/u/tmp/keep/sub"), "negation covers subfolders");
        assert!(ign("/var/lib/cache"));
        assert!(!ign("/var/lib/x/cache"), "* stays in one component");
        assert!(ign("/home/u/a/scratch/b"), "bare name at any depth");
        assert!(ign("/home/u/work/app/old"));
        assert!(!ign("/home/u"));
    }

    #[test]
    fn id_and_name_patterns() {
        let list = IgnoreList::parse("id:abc\nname:test*\n**/proj\n!name:test-keep", None);
        assert!(list.is_ignored(&session("/w", "abc", None)));
        assert!(list.is_ignored(&session("/w", "x", Some("test-1"))));
        assert!(!list.is_ignored(&session("/w", "x", Some("test-keep"))));
        assert!(!list.is_ignored(&session("/w", "x", Some("prod"))));
        assert!(list.is_ignored(&session("/a/b/proj", "x", None)));
        assert!(IgnoreList::parse("~/x", None).is_empty(), "~ without home");
    }
}
