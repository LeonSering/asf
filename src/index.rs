//! Full-text index of session conversations (user questions and agent text).
//!
//! Each session gets a plain-text file under `~/.cache/asf/text/`:
//! - `<id>.txt`: one message per line, first byte is the role (`U` or `A`),
//!   newlines inside a message are stored as `\x1f`.
//! - `<id>.len`: `<source offset> <text length>`, i.e. how far the session
//!   file has been indexed and how long the text file was at that point.
//!
//! Session files are append-only, so updates only parse the new bytes.
//! An indexer thread keeps the in-memory docs up to date and a search thread
//! answers queries against them without blocking the UI.

use crate::scanner::{Role, SessionMeta, conversation_message};
use aho_corasick::AhoCorasick;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, RwLock};
use std::time::Duration;

/// Escaped newline inside a stored message (1 byte, like `\n`).
const NL_MARK: char = '\x1f';
/// Hits kept per session for display. All hits are still counted.
const MAX_HITS: usize = 200;

#[derive(Debug, Clone)]
pub struct Msg {
    pub role: Role,
    /// Byte range of the message text in `Doc::text`
    pub range: Range<usize>,
}

/// In-memory conversation text of one session.
#[derive(Debug, Default)]
pub struct Doc {
    /// All messages, one per line (role byte + text), with real newlines
    /// restored inside messages. Byte offsets match the file on disk.
    pub text: String,
    pub msgs: Vec<Msg>,
    /// Number of user questions
    pub question_count: usize,
    /// Text lines of all messages (questions and agent replies)
    pub line_count: usize,
}

impl Doc {
    fn parse(mut raw: String) -> Self {
        let mut msgs = Vec::new();
        let mut pos = 0;
        for line in raw.split_inclusive('\n') {
            let body_end = pos + line.trim_end_matches('\n').len();
            let role = match line.as_bytes().first() {
                Some(b'U') => Some(Role::User),
                Some(b'A') => Some(Role::Assistant),
                _ => None,
            };
            if let Some(role) = role {
                msgs.push(Msg {
                    role,
                    range: pos + 1..body_end,
                });
            }
            pos += line.len();
        }
        // Same byte length, so all ranges stay valid.
        if raw.contains(NL_MARK) {
            raw = raw.replace(NL_MARK, "\n");
        }
        let question_count = msgs.iter().filter(|m| m.role == Role::User).count();
        let line_count = msgs
            .iter()
            .map(|m| raw[m.range.clone()].matches('\n').count() + 1)
            .sum();
        Self {
            text: raw,
            msgs,
            question_count,
            line_count,
        }
    }

    pub fn questions(&self) -> impl Iterator<Item = &str> {
        self.msgs
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| &self.text[m.range.clone()])
    }

    /// Index of the message containing byte offset `pos`.
    fn msg_at(&self, pos: usize) -> Option<usize> {
        let i = self.msgs.partition_point(|m| m.range.end < pos);
        self.msgs.get(i).filter(|m| m.range.start <= pos).map(|_| i)
    }
}

/// Bump when the extracted text changes, so all sessions are re-indexed.
const INDEX_VERSION: u32 = 2;

fn cache_root() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("asf")
}

fn index_dir() -> PathBuf {
    cache_root().join(format!("text-v{INDEX_VERSION}"))
}

/// Delete index directories from older index versions.
fn remove_old_index_dirs() {
    let Ok(entries) = fs::read_dir(cache_root()) else {
        return;
    };
    let current = format!("text-v{INDEX_VERSION}");
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if (name == "text" || name.starts_with("text-v")) && name != current {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

fn paths(dir: &Path, session_id: &str) -> (PathBuf, PathBuf) {
    (
        dir.join(format!("{session_id}.txt")),
        dir.join(format!("{session_id}.len")),
    )
}

fn encode(role: Role, text: &str, out: &mut String) {
    out.push(match role {
        Role::User => 'U',
        Role::Assistant => 'A',
    });
    for c in text.chars() {
        match c {
            '\n' => out.push(NL_MARK),
            '\r' | NL_MARK => {}
            c => out.push(c),
        }
    }
    out.push('\n');
}

/// Bring the index file of one session up to date and load it.
fn update_and_load(dir: &Path, session: &SessionMeta) -> std::io::Result<Doc> {
    let (txt_path, len_path) = paths(dir, &session.session_id);

    let (mut src_off, mut txt_len) = fs::read_to_string(&len_path)
        .ok()
        .and_then(|s| {
            let mut it = s.split_whitespace().map(|n| n.parse::<u64>().ok());
            Some((it.next()??, it.next()??))
        })
        .unwrap_or((0, 0));

    let src_len = fs::metadata(&session.file_path)?.len();
    if src_off > src_len {
        // Session file was rewritten: start over.
        src_off = 0;
        txt_len = 0;
    }

    let mut txt = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&txt_path)?;
    // Drop anything written after the last recorded state (e.g. a crash
    // between appending text and updating the .len file).
    if txt.metadata()?.len() != txt_len {
        txt.set_len(txt_len)?;
    }

    if src_off < src_len {
        let mut src = fs::File::open(&session.file_path)?;
        src.seek(SeekFrom::Start(src_off))?;
        let mut buf = Vec::new();
        src.read_to_end(&mut buf)?;

        // Only complete lines; a trailing partial line is still being written.
        if let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') {
            let mut out = String::new();
            for line in buf[..=last_nl].split(|&b| b == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let line = String::from_utf8_lossy(line);
                if let Some((role, text)) = conversation_message(&line) {
                    encode(role, &text, &mut out);
                }
            }
            txt.seek(SeekFrom::Start(txt_len))?;
            txt.write_all(out.as_bytes())?;
            src_off += last_nl as u64 + 1;
            txt_len += out.len() as u64;

            let tmp = len_path.with_extension("len.tmp");
            fs::write(&tmp, format!("{src_off} {txt_len}"))?;
            fs::rename(&tmp, &len_path)?;
        }
    }

    let mut raw = Vec::with_capacity(txt_len as usize);
    txt.seek(SeekFrom::Start(0))?;
    txt.take(txt_len).read_to_end(&mut raw)?;
    Ok(Doc::parse(String::from_utf8_lossy(&raw).into_owned()))
}

/// Remove the index files of a deleted session.
pub fn remove(session_id: &str) {
    let (txt, len) = paths(&index_dir(), session_id);
    let _ = fs::remove_file(txt);
    let _ = fs::remove_file(len);
}

/// Index shared between the indexer thread, the search thread and the UI.
pub struct SharedIndex {
    docs: RwLock<HashMap<String, Arc<Doc>>>,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    /// Bumped whenever docs change, so searches can be re-run.
    version: AtomicU64,
}

impl SharedIndex {
    pub fn doc(&self, session_id: &str) -> Option<Arc<Doc>> {
        self.docs.read().ok()?.get(session_id).cloned()
    }

    pub fn forget(&self, session_id: &str) {
        if let Ok(mut docs) = self.docs.write() {
            docs.remove(session_id);
        }
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    pub fn is_indexing(&self) -> bool {
        self.done.load(Ordering::Relaxed) < self.total.load(Ordering::Relaxed)
    }
}

/// Start indexing the sessions in the background (in the given order),
/// except those in `skip`. Index files of skipped sessions are kept, so they
/// load quickly if they are indexed later with `index_more`.
pub fn spawn_indexer(sessions: &[SessionMeta], skip: &HashSet<String>) -> Arc<SharedIndex> {
    let index = Arc::new(SharedIndex {
        docs: RwLock::new(HashMap::new()),
        done: AtomicUsize::new(0),
        total: AtomicUsize::new(0),
        version: AtomicU64::new(0),
    });
    let known: HashSet<String> = sessions.iter().map(|s| s.session_id.clone()).collect();
    let todo: Vec<SessionMeta> = sessions
        .iter()
        .filter(|s| !skip.contains(&s.session_id))
        .cloned()
        .collect();
    index.total.store(todo.len(), Ordering::Relaxed);

    let shared = Arc::clone(&index);
    std::thread::spawn(move || {
        remove_old_index_dirs();
        let dir = index_dir();
        if fs::create_dir_all(&dir).is_err() {
            shared.done.fetch_add(todo.len(), Ordering::Relaxed);
            return;
        }
        index_into(&shared, &dir, &todo);

        // Remove index files of sessions that no longer exist.
        if let Ok(entries) = fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
                let ours = matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("txt" | "len" | "tmp")
                );
                if ours && !known.contains(stem) {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    });

    index
}

/// Index more sessions in the background (e.g. ignored ones when shown).
pub fn index_more(index: &Arc<SharedIndex>, sessions: Vec<SessionMeta>) {
    index.total.fetch_add(sessions.len(), Ordering::Relaxed);
    let shared = Arc::clone(index);
    std::thread::spawn(move || {
        let dir = index_dir();
        if fs::create_dir_all(&dir).is_err() {
            shared.done.fetch_add(sessions.len(), Ordering::Relaxed);
            return;
        }
        index_into(&shared, &dir, &sessions);
    });
}

fn index_into(shared: &SharedIndex, dir: &Path, sessions: &[SessionMeta]) {
    for s in sessions {
        if let Ok(doc) = update_and_load(dir, s) {
            if let Ok(mut docs) = shared.docs.write() {
                docs.insert(s.session_id.clone(), Arc::new(doc));
            }
            shared.version.fetch_add(1, Ordering::Relaxed);
        }
        shared.done.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub msg: usize,
    /// Byte range of the match in `Doc::text`
    pub range: (usize, usize),
}

#[derive(Debug, Default)]
pub struct SessionHits {
    pub count: usize,
    /// First MAX_HITS hits in conversation order
    pub hits: Vec<Hit>,
}

#[derive(Debug, Default)]
pub struct SearchResult {
    pub query: String,
    pub sessions: HashMap<String, SessionHits>,
}

fn search(index: &SharedIndex, query: &str) -> SearchResult {
    let mut result = SearchResult {
        query: query.to_string(),
        ..Default::default()
    };
    let Ok(ac) = AhoCorasick::builder()
        .ascii_case_insensitive(true)
        .build([query])
    else {
        return result;
    };

    let docs: Vec<(String, Arc<Doc>)> = match index.docs.read() {
        Ok(d) => d.iter().map(|(k, v)| (k.clone(), Arc::clone(v))).collect(),
        Err(_) => return result,
    };

    for (id, doc) in docs {
        let mut sh = SessionHits::default();
        for m in ac.find_iter(&doc.text) {
            // Skip matches that touch the role byte or span two messages.
            let Some(i) = doc.msg_at(m.start()) else {
                continue;
            };
            if m.end() > doc.msgs[i].range.end || m.start() < doc.msgs[i].range.start {
                continue;
            }
            sh.count += 1;
            if sh.hits.len() < MAX_HITS {
                sh.hits.push(Hit {
                    msg: i,
                    range: (m.start(), m.end()),
                });
            }
        }
        if sh.count > 0 {
            result.sessions.insert(id, sh);
        }
    }
    result
}

/// Start the search thread. Send queries in, receive results out.
/// Queries are debounced, and a search is re-run when the index changes.
pub fn spawn_searcher(index: Arc<SharedIndex>) -> (Sender<String>, Receiver<SearchResult>) {
    const DEBOUNCE: Duration = Duration::from_millis(120);
    const RECHECK: Duration = Duration::from_millis(500);

    let (query_tx, query_rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::channel();

    std::thread::spawn(move || {
        let mut query = String::new();
        let mut last: Option<(String, u64)> = None;
        loop {
            match query_rx.recv_timeout(RECHECK) {
                Ok(q) => {
                    query = q;
                    // Wait until typing pauses; keep only the newest query.
                    while let Ok(q) = query_rx.recv_timeout(DEBOUNCE) {
                        query = q;
                    }
                    // Always answer an explicit query, even if it equals the
                    // last one searched (e.g. "abc" -> "" -> "abc"): the UI
                    // waits for a result for it.
                    last = None;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }

            let version = index.version.load(Ordering::Relaxed);
            if query.is_empty() || last.as_ref() == Some(&(query.clone(), version)) {
                continue;
            }
            let result = search(&index, &query);
            if result_tx.send(result).is_err() {
                return;
            }
            last = Some((query.clone(), version));
        }
    });

    (query_tx, result_rx)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_roundtrip_and_hit_mapping() {
        let mut raw = String::new();
        encode(Role::User, "fix the\nTMPDIR bug", &mut raw);
        encode(Role::Assistant, "Done, tmpdir is used now", &mut raw);
        let doc = Doc::parse(raw);
        assert_eq!(doc.msgs.len(), 2);
        assert_eq!((doc.question_count, doc.line_count), (1, 3));
        assert_eq!(
            &doc.text[doc.msgs[1].range.clone()],
            "Done, tmpdir is used now"
        );
        assert_eq!(doc.questions().collect::<Vec<_>>(), ["fix the\nTMPDIR bug"]);

        let index = SharedIndex {
            docs: RwLock::new(HashMap::from([("s".to_string(), Arc::new(doc))])),
            done: AtomicUsize::new(1),
            total: AtomicUsize::new(1),
            version: AtomicU64::new(0),
        };
        let r = search(&index, "tmpdir");
        let hits = &r.sessions["s"];
        assert_eq!(hits.count, 2);
        assert_eq!(hits.hits[0].msg, 0);
        assert_eq!(hits.hits[1].msg, 1);
        // Newlines never match across messages or into the role byte.
        assert!(search(&index, "bug\nA").sessions.is_empty());
    }

    #[test]
    fn incremental_update() {
        let dir = std::env::temp_dir().join(format!("asf-index-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("sess.jsonl");
        let q = |t: &str| {
            format!(r#"{{"type":"user","message":{{"role":"user","content":"{t}"}}}}"#) + "\n"
        };
        fs::write(&src, q("first")).unwrap();
        let meta = SessionMeta {
            session_id: "sess".into(),
            file_path: src.to_string_lossy().into(),
            cwd: "/".into(),
            session_name: None,
            first_user_message: None,
            last_modified: 0,
            git_branch: None,
            file_mtime: 0,
            file_len: 0,
        };
        assert_eq!(update_and_load(&dir, &meta).unwrap().msgs.len(), 1);

        // Append one full line and one partial line.
        let mut f = fs::OpenOptions::new().append(true).open(&src).unwrap();
        write!(f, "{}{{\"type\":\"us", q("second")).unwrap();
        let doc = update_and_load(&dir, &meta).unwrap();
        assert_eq!(doc.questions().collect::<Vec<_>>(), ["first", "second"]);

        // Partial line completed later.
        writeln!(
            f,
            "er\",\"message\":{{\"role\":\"user\",\"content\":\"third\"}}}}"
        )
        .unwrap();
        let doc = update_and_load(&dir, &meta).unwrap();
        assert_eq!(
            doc.questions().collect::<Vec<_>>(),
            ["first", "second", "third"]
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}
