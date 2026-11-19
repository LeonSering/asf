use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub session_id: String,
    pub file_path: String,
    pub cwd: String,
    pub session_name: Option<String>,
    pub first_user_message: Option<String>,
    pub last_modified: u64, // seconds since epoch
    /// Git branch recorded with the latest messages
    #[serde(default)]
    pub git_branch: Option<String>,
    /// File mtime used for cache invalidation
    pub file_mtime: u64,
    /// File length when last parsed, for incremental rename detection
    #[serde(default)]
    pub file_len: u64,
}

/// Bump when parsing logic changes so stale cache entries are re-parsed.
const CACHE_VERSION: u32 = 4;

#[derive(Debug, Serialize, Deserialize, Default)]
pub struct SessionCache {
    #[serde(default)]
    pub version: u32,
    pub sessions: HashMap<String, SessionMeta>,
}

impl SessionCache {
    fn cache_path() -> PathBuf {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("~/.cache"))
            .join("asf");
        fs::create_dir_all(&cache_dir).ok();
        cache_dir.join("sessions.json")
    }

    pub fn load() -> Self {
        let path = Self::cache_path();
        if let Ok(data) = fs::read_to_string(&path) {
            let cache: Self = serde_json::from_str(&data).unwrap_or_default();
            if cache.version == CACHE_VERSION {
                return cache;
            }
            Self::default()
        } else {
            Self::default()
        }
    }

    pub fn save(&mut self) {
        self.version = CACHE_VERSION;
        let path = Self::cache_path();
        if let Ok(data) = serde_json::to_string(self) {
            fs::write(path, data).ok();
        }
    }
}

/// Scan all session files and return metadata, using cache where possible.
pub fn scan_sessions() -> Vec<SessionMeta> {
    let projects_dir = match dirs::home_dir() {
        Some(h) => h.join(".claude/projects"),
        None => return Vec::new(),
    };

    if !projects_dir.exists() {
        return Vec::new();
    }

    let mut cache = SessionCache::load();
    let mut results = Vec::new();
    let mut seen_ids = std::collections::HashSet::new();

    // Walk all project directories
    let Ok(project_entries) = fs::read_dir(&projects_dir) else {
        return Vec::new();
    };

    for project_entry in project_entries.flatten() {
        let project_path = project_entry.path();
        if !project_path.is_dir() {
            continue;
        }

        let Ok(session_files) = fs::read_dir(&project_path) else {
            continue;
        };

        for file_entry in session_files.flatten() {
            let file_path = file_entry.path();

            // Skip non-jsonl files
            if file_path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            // Skip subagent files
            if file_path
                .to_str()
                .is_some_and(|p| p.contains("/subagents/"))
            {
                continue;
            }

            let session_id = file_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();

            if session_id.is_empty() {
                continue;
            }

            seen_ids.insert(session_id.clone());

            // Check mtime for cache validity
            let mtime = file_mtime(&file_path);
            let len = fs::metadata(&file_path).map(|m| m.len()).unwrap_or(0);

            // Compare the length too: a rename appends to the file but keeps
            // its mtime, so the mtime alone would miss it.
            let prev = cache.sessions.get(&session_id);
            if let Some(cached) = prev {
                // The path check catches sessions moved to another project folder.
                if cached.file_mtime == mtime
                    && cached.file_len == len
                    && Path::new(&cached.file_path) == file_path
                {
                    results.push(cached.clone());
                    continue;
                }
            }

            // Parse the file
            if let Some(meta) = parse_session_file(&file_path, &session_id, mtime, prev) {
                cache.sessions.insert(session_id.clone(), meta.clone());
                results.push(meta);
            }
        }
    }

    // Remove stale cache entries
    cache.sessions.retain(|id, _| seen_ids.contains(id));
    cache.save();

    // Sort by most recent first
    results.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    results
}

fn file_mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Parse a session JSONL file efficiently.
/// - cwd and first user question: read forward from the start
/// - session name and git branch: read the last TAIL_BYTES of the file
///
/// `prev` is the cached entry for this session (if any). Session files are
/// append-only, so the cached name covers everything up to `prev.file_len`
/// and only the newly appended bytes need to be searched for renames.
fn parse_session_file(
    path: &Path,
    session_id: &str,
    mtime: u64,
    prev: Option<&SessionMeta>,
) -> Option<SessionMeta> {
    let file_len = fs::metadata(path).map(|m| m.len()).ok()?;

    let (cwd, first_user_message) = scan_head(path);
    let cwd = cwd?;

    let (session_name, git_branch) = scan_tail(path, file_len, prev);

    Some(SessionMeta {
        session_id: session_id.to_string(),
        file_path: path.to_string_lossy().to_string(),
        cwd,
        session_name,
        first_user_message: first_user_message.map(|m| truncate_str(&m, 100)),
        last_modified: mtime,
        git_branch,
        file_mtime: mtime,
        file_len,
    })
}

/// Read forward to extract cwd and the first user question.
/// Stops once both are found, or after HEAD_LINES if cwd is already known
/// (cwd is required, so keep looking for it until EOF).
fn scan_head(path: &Path) -> (Option<String>, Option<String>) {
    const HEAD_LINES: usize = 100;

    let Ok(file) = fs::File::open(path) else {
        return (None, None);
    };

    let mut cwd: Option<String> = None;
    let mut first_user_message: Option<String> = None;

    for (line_num, line_result) in BufReader::new(file).lines().enumerate() {
        if line_num > HEAD_LINES && cwd.is_some() {
            break;
        }
        let Ok(line) = line_result else { continue };
        if line.is_empty() {
            continue;
        }

        if cwd.is_none() && line.contains("\"cwd\"") {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) {
                if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
                    cwd = Some(c.to_string());
                }
            }
        }

        if first_user_message.is_none() {
            first_user_message = user_question(&line);
        }

        if cwd.is_some() && first_user_message.is_some() {
            break;
        }
    }

    (cwd, first_user_message)
}

/// Read the last `max_bytes` of a file as text, starting at the first full line.
/// Invalid UTF-8 (e.g. a seek landing inside a multi-byte character) is
/// replaced instead of failing the whole read.
fn read_tail(path: &Path, file_len: u64, max_bytes: u64) -> Option<(u64, String)> {
    let start = file_len.saturating_sub(max_bytes);
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;

    let bytes = if start > 0 {
        match buf.iter().position(|&b| b == b'\n') {
            Some(pos) => &buf[pos + 1..],
            None => &[][..],
        }
    } else {
        &buf[..]
    };
    Some((start, String::from_utf8_lossy(bytes).into_owned()))
}

/// Find the last session name and the latest git branch in the tail of the file.
fn scan_tail(
    path: &Path,
    file_len: u64,
    prev: Option<&SessionMeta>,
) -> (Option<String>, Option<String>) {
    const TAIL_BYTES: u64 = 512 * 1024;

    let Some((start_pos, tail)) = read_tail(path, file_len, TAIL_BYTES) else {
        return (None, None);
    };

    let mut session_name: Option<String> = None;
    let mut git_branch: Option<String> = None;

    for line in tail.lines() {
        if line.is_empty() {
            continue;
        }

        if let Some(name) = parse_name_line(line) {
            session_name = Some(name);
        }

        if let Some(branch) = parse_branch(line) {
            git_branch = Some(branch);
        }
    }

    // No rename in the tail: the latest rename (if any) lies before start_pos.
    // Reuse the cached name for the part we scanned last time and only search
    // the bytes appended since then.
    if session_name.is_none() && start_pos > 0 {
        let (from, cached_name) = match prev {
            Some(p) if p.file_len > 0 && p.file_len <= file_len => {
                (p.file_len, p.session_name.clone())
            }
            _ => (0, None),
        };
        session_name = scan_range_for_name(path, from, start_pos).or(cached_name);
    }

    (session_name, git_branch)
}

/// `gitBranch` of a JSONL entry, if present and non-empty.
fn parse_branch(line: &str) -> Option<String> {
    if !line.contains("\"gitBranch\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let b = v.get("gitBranch")?.as_str()?;
    (!b.is_empty()).then(|| b.to_string())
}

/// Forward scan of the byte range [from, to) for the last session name.
fn scan_range_for_name(path: &Path, from: u64, to: u64) -> Option<String> {
    if from >= to {
        return None;
    }
    let mut file = fs::File::open(path).ok()?;
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut reader = BufReader::new(file);

    let mut session_name = None;
    let mut pos = from;
    let mut buf = Vec::new();
    while pos < to {
        buf.clear();
        let n = reader.read_until(b'\n', &mut buf).ok()?;
        if n == 0 {
            break;
        }
        pos += n as u64;
        let line = String::from_utf8_lossy(&buf);
        if let Some(name) = parse_name_line(&line) {
            session_name = Some(name);
        }
    }
    session_name
}

fn parse_name_line(line: &str) -> Option<String> {
    if !line.contains("\"agent-name\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line.trim_end()).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("agent-name") {
        return None;
    }
    v.get("agentName")
        .and_then(|n| n.as_str())
        .map(str::to_string)
}

/// Extract text content from a message value.
fn extract_message_text(msg: &serde_json::Value) -> String {
    // Content can be a string or an array of content blocks
    if let Some(content) = msg.get("content") {
        if let Some(s) = content.as_str() {
            return s.trim().to_string();
        }
        if let Some(arr) = content.as_array() {
            let mut parts = Vec::new();
            for item in arr {
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    parts.push(text.trim().to_string());
                }
            }
            return parts.join(" ");
        }
    }
    String::new()
}

/// Remove every complete `<system-reminder>...</system-reminder>` block.
/// An opening tag without a closing tag is kept as plain text (e.g. a user
/// who typed "<system-reminder>" in a question).
fn strip_system_reminders(text: &str) -> String {
    const OPEN: &str = "<system-reminder>";
    const CLOSE: &str = "</system-reminder>";

    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(OPEN) {
        out.push_str(&rest[..start]);
        match rest[start..].find(CLOSE) {
            Some(end) => rest = &rest[start + end + CLOSE.len()..],
            None => {
                // Unclosed: keep the rest (already pushed the part before it).
                rest = &rest[start..];
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_string()
}

/// If the JSONL line is a question typed by the user, return its text with
/// system reminders removed. Tool results, meta/compaction entries, slash
/// command wrappers, task notifications and interrupt markers are skipped.
fn user_question(line: &str) -> Option<String> {
    if !line.contains("\"type\":\"user\"") || line.contains("\"toolUseResult\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("user") {
        return None;
    }
    let flag = |k: &str| v.get(k).and_then(|f| f.as_bool()).unwrap_or(false);
    if flag("isMeta")
        || flag("isCompactSummary")
        || flag("isVisibleInTranscriptOnly")
        || flag("isSidechain")
    {
        return None;
    }
    let msg = v.get("message")?;
    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
        return None;
    }

    let text = strip_system_reminders(&extract_message_text(msg));
    const SKIP_PREFIXES: [&str; 6] = [
        "<command-",
        "<local-command-",
        "<task-notification>",
        "[Request interrupted",
        "[Your previous response",
        "Caveat: The messages below",
    ];
    if text.is_empty() || SKIP_PREFIXES.iter().any(|p| text.starts_with(p)) {
        return None;
    }
    Some(text)
}

/// Truncate to at most `max_bytes` (on a char boundary), appending "..." if cut.
pub fn truncate_str(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

/// Who wrote a message in the conversation tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
}

/// If the JSONL line is assistant output with text, return that text.
/// Tool calls and thinking blocks have no `text` field and are skipped.
fn assistant_text(line: &str) -> Option<String> {
    if !line.contains("\"type\":\"assistant\"") || !line.contains("\"text\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("assistant")
        || v.get("isSidechain").and_then(|f| f.as_bool()) == Some(true)
    {
        return None;
    }
    let text = extract_message_text(v.get("message")?);
    (!text.is_empty()).then_some(text)
}

/// A user question or assistant text from one JSONL line.
pub fn conversation_message(line: &str) -> Option<(Role, String)> {
    user_question(line)
        .map(|t| (Role::User, t))
        .or_else(|| assistant_text(line).map(|t| (Role::Assistant, t)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn strips_all_system_reminders() {
        let t =
            "<system-reminder>a</system-reminder>\nhello <system-reminder>b</system-reminder>world";
        assert_eq!(strip_system_reminders(t), "hello world");
        assert_eq!(
            strip_system_reminders("remove all <system-reminder>. Only questions"),
            "remove all <system-reminder>. Only questions"
        );
    }

    #[test]
    fn user_question_filters_noise() {
        let q = r#"{"type":"user","message":{"role":"user","content":"<system-reminder>x</system-reminder>fix it"}}"#;
        assert_eq!(user_question(q).as_deref(), Some("fix it"));
        let meta = r#"{"type":"user","isMeta":true,"message":{"role":"user","content":"hi"}}"#;
        assert_eq!(user_question(meta), None);
        let tool = r#"{"type":"user","toolUseResult":{},"message":{"role":"user","content":[{"type":"tool_result"}]}}"#;
        assert_eq!(user_question(tool), None);
        let only_reminder = r#"{"type":"user","message":{"role":"user","content":"<system-reminder>x</system-reminder>"}}"#;
        assert_eq!(user_question(only_reminder), None);
    }

    #[test]
    fn read_tail_survives_mid_char_seek() {
        let dir = std::env::temp_dir().join(format!("asf-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.jsonl");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(f, "äääää").unwrap();
        writeln!(f, "last").unwrap();
        drop(f);
        let len = fs::metadata(&path).unwrap().len();
        // Start one byte into the second 'ä' (inside a 2-byte char).
        let (_, tail) = read_tail(&path, len, len - 3).unwrap();
        assert_eq!(tail, "last\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn truncate_respects_char_boundary() {
        assert_eq!(truncate_str("ääb", 3), "ä...");
        assert_eq!(truncate_str("abc", 3), "abc");
    }
}
