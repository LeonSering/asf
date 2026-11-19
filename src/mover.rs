//! Move a session to another working directory.
//!
//! Claude Code stores a session as `~/.claude/projects/<dir>/<id>.jsonl`
//! (plus an optional `<id>/` folder for tool results and subagents), where
//! `<dir>` is the working directory with every non-alphanumeric character
//! replaced by `-`. Each entry also records its `"cwd"`. Moving a session
//! therefore means:
//! 1. writing a copy into the new project folder with the old cwd replaced by
//!    the new one, keeping the modification time. Entries recorded in other
//!    folders (e.g. after the agent ran `cd`) are left as they are,
//! 2. moving the `<id>/` folder next to it,
//! 3. removing the old file, only after both steps succeeded.
//!
//! The `"cwd":"..."` fields are replaced as text, so the rest of each line
//! stays byte-for-byte identical. Text inside messages is never touched.

use std::fs;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};

/// Project folder name Claude Code uses for a working directory.
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Move the session file at `file_path` (recorded cwd `old_cwd`) so it
/// belongs to `new_cwd`. Returns the new file path.
pub fn move_session(file_path: &Path, old_cwd: &str, new_cwd: &str) -> Result<PathBuf, String> {
    let file_name = file_path.file_name().ok_or("invalid session path")?;
    let id = file_path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("invalid session path")?;
    let old_dir = file_path.parent().ok_or("invalid session path")?;
    let projects = old_dir.parent().ok_or("invalid session path")?;

    let new_dir = projects.join(project_dir_name(new_cwd));
    if new_dir == old_dir {
        return Err("the session already belongs to this folder".into());
    }
    let new_file = new_dir.join(file_name);
    let old_side = old_dir.join(id);
    let new_side = new_dir.join(id);
    if new_file.exists() || new_side.exists() {
        return Err(format!("{} already exists", new_file.display()));
    }
    fs::create_dir_all(&new_dir)
        .map_err(|e| format!("cannot create {}: {e}", new_dir.display()))?;

    // 1. Rewritten copy, written to a temp file and renamed into place.
    let meta = fs::metadata(file_path).map_err(|e| e.to_string())?;
    let tmp = new_dir.join(format!(".{id}.jsonl.asf-tmp"));
    let result = write_rewritten(file_path, &tmp, old_cwd, new_cwd).and_then(|()| {
        let f = fs::OpenOptions::new()
            .write(true)
            .open(&tmp)
            .map_err(|e| e.to_string())?;
        if let Ok(mtime) = meta.modified() {
            f.set_modified(mtime).map_err(|e| e.to_string())?;
        }
        f.sync_all().map_err(|e| e.to_string())?;
        // The session must not have changed while we copied it.
        let now = fs::metadata(file_path).map_err(|e| e.to_string())?;
        if now.len() != meta.len() || now.modified().ok() != meta.modified().ok() {
            return Err("the session file changed during the move".into());
        }
        fs::rename(&tmp, &new_file).map_err(|e| e.to_string())
    });
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(format!("move failed, nothing changed: {e}"));
    }

    // 2. Side folder (tool results, subagents, title).
    if old_side.is_dir() {
        if let Err(e) = fs::rename(&old_side, &new_side) {
            let _ = fs::remove_file(&new_file);
            return Err(format!("move failed, nothing changed: {e}"));
        }
    }

    // 3. Old file. If this fails, the session exists twice but nothing is lost.
    fs::remove_file(file_path).map_err(|e| {
        format!(
            "moved, but could not remove the old file {}: {e}",
            file_path.display()
        )
    })?;

    // Leave no empty project folder behind.
    let _ = fs::remove_dir(old_dir);
    Ok(new_file)
}

fn write_rewritten(src: &Path, dst: &Path, old_cwd: &str, new_cwd: &str) -> Result<(), String> {
    let json = |s: &str| serde_json::to_string(s).expect("string serializes");
    // `"cwd":"<old>"` exactly, with JSON escaping applied.
    let exact_old = format!("\"cwd\":{}", json(old_cwd));
    let exact_new = format!("\"cwd\":{}", json(new_cwd));

    let input = fs::File::open(src).map_err(|e| e.to_string())?;
    let output = fs::File::create(dst).map_err(|e| e.to_string())?;
    let mut out = BufWriter::new(output);
    let mut reader = BufReader::new(input);
    let mut line = Vec::new();
    loop {
        line.clear();
        let n = reader
            .read_until(b'\n', &mut line)
            .map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        // Lines that aren't valid UTF-8 are copied unchanged.
        match std::str::from_utf8(&line) {
            Ok(text) if text.contains("\"cwd\":") => {
                let text = text.replace(&exact_old, &exact_new);
                out.write_all(text.as_bytes())
            }
            _ => out.write_all(&line),
        }
        .map_err(|e| e.to_string())?;
    }
    out.flush().map_err(|e| e.to_string())
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(input: &str) -> PathBuf {
    let home = dirs::home_dir();
    match (input.strip_prefix('~'), home) {
        (Some(""), Some(h)) => h,
        (Some(rest), Some(h)) if rest.starts_with('/') => h.join(&rest[1..]),
        _ => PathBuf::from(input),
    }
}

/// Tab completion for a directory path typed by the user. Completes the last
/// path component against existing subdirectories: a unique match gets a
/// trailing `/`, several matches complete to their common prefix.
pub fn complete_dir(input: &str) -> String {
    let (parent, prefix) = match input.rfind('/') {
        Some(i) => (&input[..=i], &input[i + 1..]),
        None => ("", input),
    };
    let dir = if parent.is_empty() {
        PathBuf::from(".")
    } else {
        expand_tilde(parent)
    };
    let Ok(entries) = fs::read_dir(&dir) else {
        return input.to_string();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| n.starts_with(prefix) && (prefix.starts_with('.') || !n.starts_with('.')))
        .collect();
    names.sort();
    match names.as_slice() {
        [] => input.to_string(),
        [one] => format!("{parent}{one}/"),
        [first, rest @ ..] => {
            let mut common = first.clone();
            for n in rest {
                let len = common
                    .char_indices()
                    .zip(n.chars())
                    .take_while(|((_, a), b)| a == b)
                    .last()
                    .map_or(0, |((i, a), _)| i + a.len_utf8());
                common.truncate(len);
            }
            format!("{parent}{common}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("asf-move-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn encodes_like_claude_code() {
        assert_eq!(
            project_dir_name("/home/user/projects/app-1"),
            "-home-user-projects-app-1"
        );
        assert_eq!(
            project_dir_name("/home/user/my.dir_x"),
            "-home-user-my-dir-x"
        );
    }

    #[test]
    fn moves_file_and_side_folder_and_rewrites_cwd() {
        let root = tmp("ok");
        let projects = root.join("projects");
        let old_dir = projects.join(project_dir_name("/w/old"));
        fs::create_dir_all(old_dir.join("s1/tool-results")).unwrap();
        fs::write(old_dir.join("s1/tool-results/x.txt"), "x").unwrap();
        let src = old_dir.join("s1.jsonl");
        let content = concat!(
            r#"{"cwd":"/w/old","type":"user","message":{"content":"cd \"cwd\":\"/w/old\" stays? /w/old"}}"#,
            "\n",
            r#"{"cwd":"/w/old/sub","type":"x"}"#,
            "\n",
            r#"{"cwd":"/w/older","type":"x"}"#,
            "\n",
            r#"{"type":"agent-name","agentName":"n"}"#,
            "\n",
        );
        fs::write(&src, content).unwrap();
        let mtime =
            std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        fs::File::options()
            .write(true)
            .open(&src)
            .unwrap()
            .set_modified(mtime)
            .unwrap();

        let new = move_session(&src, "/w/old", "/w/new").unwrap();
        assert_eq!(new, projects.join("-w-new/s1.jsonl"));
        assert!(!src.exists());
        assert!(!old_dir.exists(), "empty old project folder removed");
        assert!(projects.join("-w-new/s1/tool-results/x.txt").exists());
        assert_eq!(fs::metadata(&new).unwrap().modified().unwrap(), mtime);

        let out = fs::read_to_string(&new).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert!(lines[0].starts_with(r#"{"cwd":"/w/new","type":"user""#));
        // Text inside the message is escaped differently and stays as it was.
        assert!(lines[0].ends_with(r#"stays? /w/old"}}"#));
        assert_eq!(
            lines[1], r#"{"cwd":"/w/old/sub","type":"x"}"#,
            "other folders kept"
        );
        assert_eq!(lines[2], r#"{"cwd":"/w/older","type":"x"}"#);
        assert_eq!(lines[3], r#"{"type":"agent-name","agentName":"n"}"#);
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn refuses_when_target_exists_and_changes_nothing() {
        let root = tmp("exists");
        let old_dir = root.join("projects/-w-old");
        let new_dir = root.join("projects/-w-new");
        fs::create_dir_all(&old_dir).unwrap();
        fs::create_dir_all(&new_dir).unwrap();
        let src = old_dir.join("s1.jsonl");
        fs::write(&src, "{\"cwd\":\"/w/old\"}\n").unwrap();
        fs::write(new_dir.join("s1.jsonl"), "other").unwrap();
        assert!(move_session(&src, "/w/old", "/w/new").is_err());
        assert_eq!(fs::read_to_string(&src).unwrap(), "{\"cwd\":\"/w/old\"}\n");
        assert_eq!(
            fs::read_to_string(new_dir.join("s1.jsonl")).unwrap(),
            "other"
        );
        assert!(
            move_session(&src, "/w/old", "/w/old").is_err(),
            "same folder"
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn completes_directories() {
        let root = tmp("complete");
        for d in ["alpha", "alpine", "beta", ".hidden"] {
            fs::create_dir_all(root.join(d)).unwrap();
        }
        fs::write(root.join("alfile"), "").unwrap();
        let base = format!("{}/", root.display());
        assert_eq!(complete_dir(&format!("{base}b")), format!("{base}beta/"));
        assert_eq!(complete_dir(&format!("{base}al")), format!("{base}alp"));
        assert_eq!(
            complete_dir(&format!("{base}.h")),
            format!("{base}.hidden/")
        );
        assert_eq!(complete_dir(&format!("{base}zz")), format!("{base}zz"));
        fs::remove_dir_all(&root).unwrap();
    }
}
