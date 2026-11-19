//! Detect which sessions are currently open in a running Claude Code process.
//!
//! Claude Code writes `~/.claude/sessions/<pid>.json` for each running
//! instance, with the `sessionId`, a `status` (`busy`/`idle`) and `procStart`
//! (the process start time, field 22 of `/proc/<pid>/stat`). The files are
//! not always removed on exit, so a session only counts as open if the PID is
//! alive and its start time matches, which also rules out a reused PID.
//!
//! This format is internal to Claude Code. If it changes, no session is
//! reported as open and nothing else breaks.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    Busy,
    Idle,
}

/// Session id -> status of every session open in a live Claude process.
pub fn open_sessions() -> HashMap<String, LiveStatus> {
    let mut out = HashMap::new();
    let Some(dir) = dirs::home_dir().map(|h| h.join(".claude/sessions")) else {
        return out;
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return out;
    };
    for e in entries.flatten() {
        let path = e.path();
        // Only `<pid>.json`, not `<pid>.<hash>.key` or `*.pending-title`.
        let is_pid_json = path.extension().and_then(|x| x.to_str()) == Some("json")
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.bytes().all(|b| b.is_ascii_digit()));
        if !is_pid_json {
            continue;
        }
        if let Some((id, status)) = parse_status_file(&path, Path::new("/proc")) {
            // Same session open twice: busy wins.
            let entry = out.entry(id).or_insert(status);
            if status == LiveStatus::Busy {
                *entry = LiveStatus::Busy;
            }
        }
    }
    out
}

fn parse_status_file(path: &Path, proc_root: &Path) -> Option<(String, LiveStatus)> {
    let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(path).ok()?).ok()?;
    let pid = v.get("pid")?.as_u64()?;
    let id = v.get("sessionId")?.as_str()?.to_string();
    let proc_start = v.get("procStart")?.as_str()?;
    if process_start_time(proc_root, pid)? != proc_start {
        return None;
    }
    let status = match v.get("status").and_then(|s| s.as_str()) {
        Some("busy") => LiveStatus::Busy,
        _ => LiveStatus::Idle,
    };
    Some((id, status))
}

/// Field 22 (starttime) of `/proc/<pid>/stat`. The command name (field 2)
/// may contain spaces, so count fields after the closing parenthesis.
fn process_start_time(proc_root: &Path, pid: u64) -> Option<String> {
    let stat = fs::read_to_string(proc_root.join(pid.to_string()).join("stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // after_comm starts with field 3, so field 22 is index 19.
    after_comm.split_whitespace().nth(19).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_only_live_process_with_same_start_time() {
        let dir = std::env::temp_dir().join(format!("asf-live-test-{}", std::process::id()));
        let proc_dir = dir.join("proc/42");
        fs::create_dir_all(&proc_dir).unwrap();
        // 22 fields, comm with a space and parentheses.
        let fields: Vec<String> = (3..=22).map(|i| i.to_string()).collect();
        fs::write(
            proc_dir.join("stat"),
            format!("42 (my (cl) aude) {}", fields.join(" ")),
        )
        .unwrap();
        assert_eq!(
            process_start_time(&dir.join("proc"), 42).as_deref(),
            Some("22")
        );

        let file = dir.join("42.json");
        let write = |start: &str, status: &str| {
            fs::write(
                &file,
                format!(
                    r#"{{"pid":42,"sessionId":"s1","procStart":"{start}","status":"{status}"}}"#
                ),
            )
            .unwrap();
        };
        write("22", "busy");
        assert_eq!(
            parse_status_file(&file, &dir.join("proc")),
            Some(("s1".to_string(), LiveStatus::Busy))
        );
        write("22", "idle");
        assert_eq!(
            parse_status_file(&file, &dir.join("proc")).map(|(_, s)| s),
            Some(LiveStatus::Idle)
        );
        // PID reused by another process (different start time).
        write("99", "busy");
        assert_eq!(parse_status_file(&file, &dir.join("proc")), None);

        fs::remove_dir_all(&dir).unwrap();
    }
}
