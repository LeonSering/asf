use crate::index::{self, Doc, SearchResult, SessionHits, SharedIndex};
use crate::live::{self, LiveStatus};
use crate::mover;
use crate::scanner::{Role, SessionMeta};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, Sender};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    Date,
    DirectoryTree,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// Enter / Alt-a: the `open` command from the config
    Primary,
    /// Alt-v: the optional `alt_open` command
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMode {
    Search,
    Rename,
    /// Typing the target folder for moving the selected session
    Move,
    ConfirmDelete,
    /// The selected session is already open in another Claude process
    ConfirmOpen(OpenMode),
}

/// A visible row in the list - either a directory header or a session.
/// `depth` is the indentation level in tree view (0 in date view).
#[derive(Debug, Clone)]
pub enum ListItem {
    DirectoryHeader {
        /// Real directory path, used as the key for collapsing
        path: String,
        /// Display name (relative to the parent row, `~` for the home dir)
        name: String,
        depth: usize,
        expanded: bool,
    },
    Session(SessionMeta, usize),
}

pub struct App {
    pub sessions: Vec<SessionMeta>,
    pub filtered_items: Vec<ListItem>,
    pub selected: usize,
    pub view_mode: ViewMode,
    pub input_mode: InputMode,
    pub search_query: String,
    pub rename_buffer: String,
    pub should_quit: bool,
    pub open_session: Option<(SessionMeta, OpenMode)>,
    /// Directories collapsed in tree mode (everything else is expanded)
    pub collapsed_dirs: std::collections::HashSet<String>,
    /// Scroll offset for the list
    pub scroll_offset: usize,
    /// Full-text index of all sessions, filled by a background thread
    pub index: Arc<SharedIndex>,
    query_tx: Sender<String>,
    results_rx: Receiver<SearchResult>,
    /// Latest content search result (may lag behind the typed query)
    search_result: Option<SearchResult>,
    sent_query: String,
    /// Sessions open in a running Claude process, refreshed every second
    pub live: std::collections::HashMap<String, LiveStatus>,
    live_checked: std::time::Instant,
    /// Selected button in a confirm dialog (false = "no", the default)
    pub confirm_yes: bool,
    /// Index of the current search match in the selected session
    pub match_index: usize,
    /// Manual scroll of the conversation part, in rows relative to the
    /// default position. A Cell so drawing can clamp it.
    pub preview_scroll: std::cell::Cell<isize>,
    /// Height of the conversation part at the last draw (for page scrolling)
    preview_height: std::cell::Cell<usize>,
    /// (session id, query) the match index and scroll belong to
    preview_key: (String, String),
    /// Layout of the whole conversation while searching, cached because it
    /// only changes with the session, query, width or current match.
    search_layout: std::cell::RefCell<Option<(LayoutKey, SearchLayout)>>,
    /// Top row of the search view. None = center on the current match.
    search_scroll: std::cell::Cell<Option<usize>>,
    /// Set by Ctrl-n/Ctrl-p: scroll only if the new match is off-screen.
    reveal_match: std::cell::Cell<bool>,
    /// Target folder typed in move mode
    pub move_buffer: String,
    /// One-off message shown in the footer until the next key press
    pub status: Option<(String, bool)>,
    /// Sessions matched by the ignore file
    pub ignored: std::collections::HashSet<String>,
    /// Footer label of the Enter / Alt-a command
    pub open_label: String,
    /// Footer label of the Alt-v command, None if not configured
    pub alt_label: Option<String>,
    /// Move to the top result once the search for the typed query is done
    select_top_pending: bool,
    /// Alt-h: show ignored sessions (greyed out)
    pub show_ignored: bool,
    /// Ignored sessions have been handed to the indexer
    ignored_indexed: bool,
}

impl App {
    pub fn new(sessions: Vec<SessionMeta>, ignored: std::collections::HashSet<String>) -> Self {
        let index = index::spawn_indexer(&sessions, &ignored);
        let (query_tx, results_rx) = index::spawn_searcher(Arc::clone(&index));
        let mut app = Self {
            sessions,
            filtered_items: Vec::new(),
            selected: 0,
            view_mode: ViewMode::Date,
            input_mode: InputMode::Search,
            search_query: String::new(),
            rename_buffer: String::new(),
            should_quit: false,
            open_session: None,
            collapsed_dirs: std::collections::HashSet::new(),
            scroll_offset: 0,
            index,
            query_tx,
            results_rx,
            search_result: None,
            sent_query: String::new(),
            live: live::open_sessions(),
            live_checked: std::time::Instant::now(),
            confirm_yes: false,
            match_index: 0,
            preview_scroll: std::cell::Cell::new(0),
            preview_height: std::cell::Cell::new(10),
            preview_key: (String::new(), String::new()),
            search_layout: std::cell::RefCell::new(None),
            search_scroll: std::cell::Cell::new(None),
            reveal_match: std::cell::Cell::new(false),
            move_buffer: String::new(),
            status: None,
            ignored,
            open_label: "open".into(),
            alt_label: None,
            select_top_pending: false,
            show_ignored: false,
            ignored_indexed: false,
        };
        app.rebuild_list();
        app
    }

    pub fn rebuild_list(&mut self) {
        let query = self.search_query.to_lowercase();

        let filtered_sessions: Vec<&SessionMeta> = self
            .sessions
            .iter()
            .filter(|s| self.show_ignored || !self.ignored.contains(&s.session_id))
            .filter(|s| {
                if query.is_empty() {
                    return true;
                }
                let name_match = s
                    .session_name
                    .as_deref()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&query);
                let path_match = s.cwd.to_lowercase().contains(&query);
                name_match || path_match || self.content_hits(&s.session_id).is_some()
            })
            .collect();

        self.filtered_items = match self.view_mode {
            ViewMode::Date => {
                let mut ranked = filtered_sessions;
                if !query.is_empty() {
                    // Relevance: name or path match first, then more matches
                    // in the conversation, then newest first.
                    ranked.sort_by_key(|s| {
                        let desc = s
                            .session_name
                            .as_deref()
                            .unwrap_or("")
                            .to_lowercase()
                            .contains(&query)
                            || s.cwd.to_lowercase().contains(&query);
                        let hits = self.content_hits(&s.session_id).map_or(0, |h| h.count);
                        (
                            std::cmp::Reverse(desc),
                            std::cmp::Reverse(hits),
                            std::cmp::Reverse(s.last_modified),
                        )
                    });
                }
                ranked
                    .iter()
                    .map(|s| ListItem::Session((*s).clone(), 0))
                    .collect()
            }
            ViewMode::DirectoryTree => {
                // The search only filters here; the tree keeps its usual order.
                // A folder whose name matches keeps all its sessions, since
                // their paths contain the folder name.
                // While searching, show every match regardless of collapsed folders.
                let collapsed = if query.is_empty() {
                    Some(&self.collapsed_dirs)
                } else {
                    None
                };
                build_tree(&filtered_sessions, collapsed)
            }
        };

        // Clamp selection
        if self.filtered_items.is_empty() {
            self.selected = 0;
        } else if self.selected >= self.filtered_items.len() {
            self.selected = self.filtered_items.len() - 1;
        }
    }

    /// Content search hits for a session, if they belong to the current query.
    ///
    /// While a search for a new query is still running, the previous result
    /// is used, so the list and preview don't flash empty on every keystroke.
    pub fn content_hits(&self, session_id: &str) -> Option<&SessionHits> {
        self.content_result()?.sessions.get(session_id)
    }

    fn content_result(&self) -> Option<&SearchResult> {
        if self.search_query.is_empty() {
            return None;
        }
        self.search_result.as_ref()
    }

    /// Query the shown content hits belong to (may lag behind the typed one).
    pub fn content_query(&self) -> &str {
        self.content_result().map_or("", |r| r.query.as_str())
    }

    /// A content search for the typed query hasn't returned yet.
    fn search_pending(&self) -> bool {
        !self.search_query.is_empty()
            && self
                .search_result
                .as_ref()
                .is_none_or(|r| r.query != self.search_query)
    }

    /// After the query changed: rebuild now only if no content search is
    /// pending. Otherwise keep the current list until the result arrives
    /// (tick() rebuilds then).
    fn on_query_changed(&mut self) {
        // Typing moves the cursor to the top result (like fzf). If content
        // matches are still pending, that happens when they arrive.
        self.select_top_pending = true;
        if !self.search_pending() {
            self.rebuild_list();
            self.select_first_session();
            self.select_top_pending = false;
        }
    }

    /// Select the first session row (skipping folder headers in the tree).
    fn select_first_session(&mut self) {
        self.selected = self
            .filtered_items
            .iter()
            .position(|i| matches!(i, ListItem::Session(..)))
            .unwrap_or(0);
    }

    /// Called once per main-loop iteration: forward query changes to the
    /// search thread and apply finished search results.
    pub fn tick(&mut self) {
        if self.live_checked.elapsed() >= std::time::Duration::from_secs(1) {
            self.live = live::open_sessions();
            self.live_checked = std::time::Instant::now();
        }

        if self.search_query != self.sent_query {
            self.sent_query = self.search_query.clone();
            let _ = self.query_tx.send(self.sent_query.clone());
        }

        let mut latest = None;
        while let Ok(r) = self.results_rx.try_recv() {
            latest = Some(r);
        }
        if let Some(r) = latest {
            let for_current_query = r.query == self.search_query;
            self.search_result = Some(r);
            if self.select_top_pending && for_current_query {
                // First result for a new query: go to the top result.
                self.select_top_pending = false;
                self.rebuild_list();
                self.select_first_session();
                return self.reset_preview_if_changed();
            }
            // Later updates (e.g. more sessions indexed): keep the selection.
            let selected_id = self.selected_session().map(|s| s.session_id.clone());
            self.rebuild_list();
            if let Some(id) = selected_id {
                if let Some(pos) = self
                    .filtered_items
                    .iter()
                    .position(|i| matches!(i, ListItem::Session(s, _) if s.session_id == id))
                {
                    self.selected = pos;
                }
            }
        }

        self.reset_preview_if_changed();
    }

    /// New session or new search: start at the first match, no scroll.
    fn reset_preview_if_changed(&mut self) {
        let key = (
            self.selected_session()
                .map(|s| s.session_id.clone())
                .unwrap_or_default(),
            self.content_query().to_string(),
        );
        if key != self.preview_key {
            self.preview_key = key;
            self.match_index = 0;
            self.preview_scroll.set(0);
            self.search_scroll.set(None);
            self.reveal_match.set(false);
        }
    }

    /// Jump to the next (or previous) search match in the selected session.
    fn jump_match(&mut self, forward: bool) {
        let Some(n) = self
            .selected_session()
            .and_then(|s| self.content_hits(&s.session_id))
            .map(|h| h.hits.len())
        else {
            return;
        };
        if n == 0 {
            return;
        }
        let i = self.match_index.min(n - 1);
        self.match_index = if forward {
            (i + 1) % n
        } else {
            (i + n - 1) % n
        };
        self.reveal_match.set(true);
    }

    fn scroll_preview(&mut self, rows: isize) {
        self.preview_scroll.set(self.preview_scroll.get() + rows);
        if let Some(top) = self.search_scroll.get() {
            self.search_scroll
                .set(Some(top.saturating_add_signed(rows)));
        }
    }

    pub fn selected_session(&self) -> Option<&SessionMeta> {
        self.filtered_items.get(self.selected).and_then(|item| {
            if let ListItem::Session(s, _) = item {
                Some(s)
            } else {
                None
            }
        })
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        if !self.filtered_items.is_empty() && self.selected < self.filtered_items.len() - 1 {
            self.selected += 1;
        }
    }

    fn toggle_dir_or_open(&mut self, mode: OpenMode) {
        if let Some(item) = self.filtered_items.get(self.selected) {
            match item {
                ListItem::DirectoryHeader { path, .. } => {
                    let path = path.clone();
                    if !self.collapsed_dirs.remove(&path) {
                        self.collapsed_dirs.insert(path);
                    }
                    self.rebuild_list();
                }
                ListItem::Session(s, _) => {
                    // Already open elsewhere: ask first, since two processes
                    // would append to the same session file.
                    let confirmed = self.input_mode == InputMode::ConfirmOpen(mode);
                    if !confirmed && self.live.contains_key(&s.session_id) {
                        self.input_mode = InputMode::ConfirmOpen(mode);
                        self.confirm_yes = false;
                        return;
                    }
                    self.open_session = Some((s.clone(), mode));
                    self.should_quit = true;
                }
            }
        }
    }

    /// Left: collapse the selected folder, or jump to the parent folder.
    fn tree_left(&mut self) {
        let Some(item) = self.filtered_items.get(self.selected) else {
            return;
        };
        let depth = match item {
            ListItem::DirectoryHeader {
                path,
                depth,
                expanded,
                ..
            } => {
                if *expanded && self.search_query.is_empty() {
                    self.collapsed_dirs.insert(path.clone());
                    self.rebuild_list();
                    return;
                }
                *depth
            }
            ListItem::Session(_, depth) => *depth,
        };
        if let Some(parent) = self.filtered_items[..self.selected]
            .iter()
            .rposition(|i| matches!(i, ListItem::DirectoryHeader { depth: d, .. } if *d < depth))
        {
            self.selected = parent;
        }
    }

    /// Right: expand the selected folder.
    fn tree_right(&mut self) {
        if let Some(ListItem::DirectoryHeader { path, .. }) = self.filtered_items.get(self.selected)
        {
            if self.collapsed_dirs.remove(path) {
                self.rebuild_list();
            }
        }
    }

    fn handle_enter(&mut self) {
        self.toggle_dir_or_open(OpenMode::Primary);
    }

    /// Show or hide the sessions matched by the ignore file.
    fn toggle_ignored(&mut self) {
        if self.ignored.is_empty() {
            self.status = Some(("No sessions are ignored.".into(), false));
            return;
        }
        self.show_ignored = !self.show_ignored;
        if self.show_ignored && !self.ignored_indexed {
            self.ignored_indexed = true;
            let todo = self
                .sessions
                .iter()
                .filter(|s| self.ignored.contains(&s.session_id))
                .cloned()
                .collect();
            index::index_more(&self.index, todo);
        }
        let selected_id = self.selected_session().map(|s| s.session_id.clone());
        self.rebuild_list();
        if let Some(id) = selected_id {
            if let Some(pos) = self
                .filtered_items
                .iter()
                .position(|i| matches!(i, ListItem::Session(s, _) if s.session_id == id))
            {
                self.selected = pos;
            }
        }
        // Wake the search thread: newly shown sessions may match.
        self.sent_query.clear();
    }

    fn start_move(&mut self) {
        let Some(s) = self.selected_session() else {
            return;
        };
        if self.live.contains_key(&s.session_id) {
            self.status = Some((
                "This session is open in another terminal. Close it before moving.".into(),
                true,
            ));
            return;
        }
        self.move_buffer = format!("{}/", tilde_path(&s.cwd).trim_end_matches('/'));
        self.input_mode = InputMode::Move;
    }

    fn confirm_move(&mut self) {
        self.input_mode = InputMode::Search;
        let Some(s) = self.selected_session().cloned() else {
            return;
        };
        let typed = self.move_buffer.trim();
        let target = match std::fs::canonicalize(mover::expand_tilde(typed)) {
            Ok(p) if p.is_dir() => p.to_string_lossy().into_owned(),
            _ => {
                self.status = Some((format!("Not an existing folder: {typed}"), true));
                return;
            }
        };
        // Checked again: it may have been opened since the prompt appeared.
        if live::open_sessions().contains_key(&s.session_id) {
            self.status = Some(("This session is open in another terminal.".into(), true));
            return;
        }
        match mover::move_session(std::path::Path::new(&s.file_path), &s.cwd, &target) {
            Ok(new_path) => {
                for session in &mut self.sessions {
                    if session.session_id == s.session_id {
                        session.file_path = new_path.to_string_lossy().into_owned();
                        session.cwd = target.clone();
                    }
                }
                self.status = Some((
                    format!(
                        "Moved '{}' to {}",
                        s.session_name.as_deref().unwrap_or("unnamed"),
                        tilde_path(&target)
                    ),
                    false,
                ));
                self.rebuild_list();
                if let Some(pos) = self.filtered_items.iter().position(
                    |i| matches!(i, ListItem::Session(m, _) if m.session_id == s.session_id),
                ) {
                    self.selected = pos;
                }
            }
            Err(e) => self.status = Some((e, true)),
        }
    }

    fn start_rename(&mut self) {
        if let Some(s) = self.selected_session() {
            self.rename_buffer = s.session_name.clone().unwrap_or_default();
            self.input_mode = InputMode::Rename;
        }
    }

    fn confirm_rename(&mut self) {
        if let Some(s) = self.selected_session().cloned() {
            let new_name = self.rename_buffer.trim().to_string();
            if !new_name.is_empty() {
                // Write a new agent-name entry to the session file
                let entry = serde_json::json!({
                    "type": "agent-name",
                    "agentName": new_name,
                    "sessionId": s.session_id,
                });
                if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&s.file_path) {
                    use std::io::Write;
                    // Keep the modification time, so a rename doesn't count
                    // as activity and the session keeps its place in the list.
                    let mtime = file.metadata().and_then(|m| m.modified()).ok();
                    if writeln!(file, "{entry}").is_ok() {
                        if let Some(t) = mtime {
                            let _ = file.set_modified(t);
                        }
                    }
                }

                // Update in-memory
                for session in &mut self.sessions {
                    if session.session_id == s.session_id {
                        session.session_name = Some(new_name.clone());
                        break;
                    }
                }
            }
            self.input_mode = InputMode::Search;
            self.rebuild_list();
        }
    }

    fn start_delete(&mut self) {
        if self.selected_session().is_some() {
            self.input_mode = InputMode::ConfirmDelete;
            self.confirm_yes = false;
        }
    }

    fn confirm_delete(&mut self) {
        if let Some(s) = self.selected_session().cloned() {
            // Delete the file
            std::fs::remove_file(&s.file_path).ok();
            index::remove(&s.session_id);
            self.index.forget(&s.session_id);
            // Remove from in-memory list
            self.sessions.retain(|ss| ss.session_id != s.session_id);
            self.input_mode = InputMode::Search;
            self.rebuild_list();
        }
    }

    /// Key handling inside a confirm dialog. Left/Right (and Tab, h/l) move
    /// between the buttons, Enter picks the selected one, y/n answer
    /// directly and Esc means no. Returns the answer once one is given.
    fn confirm_key(&mut self, key: KeyEvent) -> Option<bool> {
        match key.code {
            KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
                self.confirm_yes = !self.confirm_yes;
                None
            }
            KeyCode::Char('h') => {
                self.confirm_yes = true;
                None
            }
            KeyCode::Char('l') => {
                self.confirm_yes = false;
                None
            }
            KeyCode::Enter => Some(self.confirm_yes),
            KeyCode::Char('y' | 'Y') => Some(true),
            KeyCode::Char('n' | 'N') | KeyCode::Esc => Some(false),
            _ => None,
        }
    }

    pub fn handle_event(&mut self, event: Event) {
        if let Event::Key(key) = event {
            self.status = None;
            match self.input_mode {
                InputMode::ConfirmDelete => {
                    match self.confirm_key(key) {
                        Some(true) => self.confirm_delete(),
                        Some(false) => self.input_mode = InputMode::Search,
                        None => {}
                    }
                    return;
                }
                InputMode::ConfirmOpen(mode) => {
                    if let Some(yes) = self.confirm_key(key) {
                        if yes {
                            self.toggle_dir_or_open(mode);
                        }
                        self.input_mode = InputMode::Search;
                    }
                    return;
                }
                InputMode::Move => {
                    match key.code {
                        KeyCode::Enter => self.confirm_move(),
                        KeyCode::Esc => self.input_mode = InputMode::Search,
                        KeyCode::Tab => self.move_buffer = mover::complete_dir(&self.move_buffer),
                        KeyCode::Backspace => {
                            self.move_buffer.pop();
                        }
                        KeyCode::Char(c) => self.move_buffer.push(c),
                        _ => {}
                    }
                    return;
                }
                InputMode::Rename => {
                    match key.code {
                        KeyCode::Enter => self.confirm_rename(),
                        KeyCode::Esc => self.input_mode = InputMode::Search,
                        KeyCode::Backspace => {
                            self.rename_buffer.pop();
                        }
                        KeyCode::Char(c) => self.rename_buffer.push(c),
                        _ => {}
                    }
                    return;
                }
                _ => {}
            }

            self.handle_normal_key(key);
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.should_quit = true,
            KeyCode::Up if key.modifiers.contains(KeyModifiers::CONTROL) => self.scroll_preview(-3),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_preview(3)
            }
            KeyCode::PageUp if key.modifiers.contains(KeyModifiers::ALT) => {
                let page = self.preview_height.get().saturating_sub(2).max(1) as isize;
                self.scroll_preview(-page);
            }
            KeyCode::PageDown if key.modifiers.contains(KeyModifiers::ALT) => {
                let page = self.preview_height.get().saturating_sub(2).max(1) as isize;
                self.scroll_preview(page);
            }
            KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_match(true);
            }
            KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.jump_match(false);
            }
            KeyCode::Up => self.move_up(),
            KeyCode::Down => self.move_down(),
            KeyCode::PageUp => {
                self.selected = self.selected.saturating_sub(20);
            }
            KeyCode::PageDown => {
                if !self.filtered_items.is_empty() {
                    self.selected = (self.selected + 20).min(self.filtered_items.len() - 1);
                }
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => {
                if !self.filtered_items.is_empty() {
                    self.selected = self.filtered_items.len() - 1;
                }
            }
            KeyCode::Enter => self.handle_enter(),
            KeyCode::Left if self.view_mode == ViewMode::DirectoryTree => self.tree_left(),
            KeyCode::Right if self.view_mode == ViewMode::DirectoryTree => self.tree_right(),
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.toggle_dir_or_open(OpenMode::Primary);
            }
            KeyCode::Char('v') if key.modifiers.contains(KeyModifiers::ALT) => {
                if self.alt_label.is_some() {
                    self.toggle_dir_or_open(OpenMode::Alt);
                } else {
                    self.status = Some((
                        "No alt_open command configured (see ~/.config/asf/config).".into(),
                        true,
                    ));
                }
            }
            KeyCode::Tab => {
                self.view_mode = match self.view_mode {
                    ViewMode::Date => ViewMode::DirectoryTree,
                    ViewMode::DirectoryTree => ViewMode::Date,
                };
                self.rebuild_list();
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_down();
            }
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.move_up();
            }
            KeyCode::Char('m') if key.modifiers.contains(KeyModifiers::ALT) => self.start_move(),
            KeyCode::Char('h') if key.modifiers.contains(KeyModifiers::ALT) => {
                self.toggle_ignored();
            }
            KeyCode::Char('r') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_rename();
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.start_delete();
            }
            KeyCode::Backspace => {
                self.search_query.pop();
                self.on_query_changed();
            }
            KeyCode::Char(c) => {
                // j/k navigate when search field is empty
                if c == 'k' && self.search_query.is_empty() {
                    self.move_up();
                } else if c == 'j' && self.search_query.is_empty() {
                    self.move_down();
                } else {
                    self.search_query.push(c);
                    self.on_query_changed();
                }
            }
            _ => {}
        }
    }
}

// Directory tree

#[derive(Default)]
struct DirNode<'a> {
    /// Display name relative to the parent row
    name: String,
    /// Real path of the directory
    path: String,
    sessions: Vec<&'a SessionMeta>,
    children: BTreeMap<String, DirNode<'a>>,
}

impl<'a> DirNode<'a> {
    fn count(&self) -> usize {
        self.sessions.len() + self.children.values().map(DirNode::count).sum::<usize>()
    }

    fn most_recent(&self) -> u64 {
        let own = self.sessions.iter().map(|s| s.last_modified).max();
        let kids = self.children.values().map(DirNode::most_recent).max();
        own.max(kids).unwrap_or(0)
    }

    /// Merge folders that hold no sessions and have a single subfolder into
    /// one row (`a/b/c/`), bottom-up.
    fn compact(&mut self) {
        for child in self.children.values_mut() {
            child.compact();
        }
        while self.sessions.is_empty() && self.children.len() == 1 {
            let (_, child) = self.children.pop_first().expect("one child");
            self.name = join_name(&self.name, &child.name);
            self.path = child.path;
            self.sessions = child.sessions;
            self.children = child.children;
        }
    }

    fn emit(
        &self,
        depth: usize,
        collapsed: Option<&std::collections::HashSet<String>>,
        out: &mut Vec<ListItem>,
    ) {
        let expanded = collapsed.is_none_or(|c| !c.contains(&self.path));
        out.push(ListItem::DirectoryHeader {
            path: self.path.clone(),
            name: format!("{}/", self.name.trim_end_matches('/')),
            depth,
            expanded,
        });
        if expanded {
            self.emit_contents(depth + 1, collapsed, out);
        }
    }

    /// Sessions first (newest first), then subfolders (most recent
    /// activity first), all at the same depth.
    fn emit_contents(
        &self,
        depth: usize,
        collapsed: Option<&std::collections::HashSet<String>>,
        out: &mut Vec<ListItem>,
    ) {
        let mut sessions = self.sessions.clone();
        sessions.sort_by_key(|s| std::cmp::Reverse(s.last_modified));
        for s in sessions {
            out.push(ListItem::Session(s.clone(), depth));
        }
        let mut kids: Vec<&DirNode> = self.children.values().collect();
        kids.sort_by_key(|k| std::cmp::Reverse(k.most_recent()));
        for k in kids {
            k.emit(depth, collapsed, out);
        }
    }
}

/// Show paths under the home directory as `~/...`.
fn tilde_path(path: &str) -> String {
    let Some(home) = dirs::home_dir() else {
        return path.to_string();
    };
    match std::path::Path::new(path).strip_prefix(&home) {
        Ok(rel) if rel.as_os_str().is_empty() => "~".to_string(),
        Ok(rel) => format!("~/{}", rel.to_string_lossy()),
        Err(_) => path.to_string(),
    }
}

fn join_name(parent: &str, child: &str) -> String {
    if parent.ends_with('/') {
        format!("{parent}{child}")
    } else {
        format!("{parent}/{child}")
    }
}

/// Build the tree view: paths under the home directory hang off `~`, all
/// others off `/`. A root without sessions of its own is not shown as a row;
/// its subfolders become top-level rows (`~/abc/`, `~/def/`).
fn build_tree(
    sessions: &[&SessionMeta],
    collapsed: Option<&std::collections::HashSet<String>>,
) -> Vec<ListItem> {
    use std::path::Path;

    let home = dirs::home_dir();
    let mut home_root = DirNode {
        name: "~".into(),
        path: home
            .as_ref()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default(),
        ..Default::default()
    };
    let mut fs_root = DirNode {
        name: "/".into(),
        path: "/".into(),
        ..Default::default()
    };

    for s in sessions {
        let cwd = Path::new(&s.cwd);
        let (root, rel) = match home.as_deref().and_then(|h| cwd.strip_prefix(h).ok()) {
            Some(rel) => (&mut home_root, rel),
            None => (&mut fs_root, cwd.strip_prefix("/").unwrap_or(cwd)),
        };
        let mut node = root;
        let mut path = std::path::PathBuf::from(&node.path);
        for comp in rel.iter() {
            let comp = comp.to_string_lossy().into_owned();
            path.push(&comp);
            node = node
                .children
                .entry(comp.clone())
                .or_insert_with(|| DirNode {
                    name: comp,
                    path: path.to_string_lossy().into_owned(),
                    ..Default::default()
                });
        }
        node.sessions.push(s);
    }

    let mut tops: Vec<DirNode> = Vec::new();
    for mut root in [home_root, fs_root] {
        if root.count() == 0 {
            continue;
        }
        root.compact();
        if root.sessions.is_empty() {
            // Lift the subfolders, keeping the root as name prefix.
            let label = root.name.clone();
            for (_, mut child) in std::mem::take(&mut root.children) {
                child.name = join_name(&label, &child.name);
                tops.push(child);
            }
        } else {
            tops.push(root);
        }
    }
    tops.sort_by_key(|t| std::cmp::Reverse(t.most_recent()));

    let mut items = Vec::new();
    for t in &tops {
        t.emit(0, collapsed, &mut items);
    }
    items
}

// Rendering

// Accent color matching fzf theme (#d97757)
const ACCENT: Color = Color::Rgb(217, 119, 87);

pub fn draw(frame: &mut ratatui::Frame, app: &mut App) {
    let area = frame.area();

    let chunks = Layout::vertical([
        Constraint::Length(3), // search bar (bordered)
        Constraint::Min(3),    // main content area
        Constraint::Length(3), // footer (bordered)
    ])
    .split(area);

    draw_search_bar(frame, chunks[0], app);

    // Split main area: list on left, preview on right
    let main_chunks = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(chunks[1]);

    draw_session_list(frame, main_chunks[0], app);
    draw_preview(frame, main_chunks[1], app);
    draw_footer(frame, chunks[2], app);
    draw_confirm_popup(frame, area, app);
}

/// Centered dialog for the delete and "already open" confirmations.
fn draw_confirm_popup(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let name = app
        .selected_session()
        .and_then(|s| s.session_name.as_deref())
        .unwrap_or("unnamed");
    let (title, question) = match app.input_mode {
        InputMode::ConfirmDelete => ("delete session", format!("Delete '{name}'?")),
        InputMode::ConfirmOpen(_) => (
            "session already open",
            format!("'{name}' is already open in another terminal. Open anyway?"),
        ),
        _ => return,
    };

    let button = |label: &str, selected: bool| {
        let style = if selected {
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Span::styled(format!("[ {label} ]"), style)
    };
    let text = vec![
        Line::raw(""),
        Line::styled(question, Style::default().fg(Color::Yellow)),
        Line::raw(""),
        Line::from(vec![
            button("yes", app.confirm_yes),
            Span::raw("    "),
            button("no", !app.confirm_yes),
        ]),
    ];

    // Up to 60 columns wide (never wider than the terminal), tall enough for the
    // wrapped question.
    let width = area.width.min(60);
    let para = Paragraph::new(text)
        .alignment(Alignment::Center)
        .wrap(Wrap { trim: true })
        .block(bordered_block(title));
    let height = (para.line_count(width) as u16).min(area.height);
    let popup = Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, popup);
    frame.render_widget(para, popup);
}

fn bordered_block(title: &str) -> Block<'static> {
    let title_span = Span::styled(
        format!(" {title} "),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    );
    Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(ACCENT))
        .title(title_span)
}

fn draw_search_bar(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let count = app
        .filtered_items
        .iter()
        .filter(|i| matches!(i, ListItem::Session(..)))
        .count();
    let total = app
        .sessions
        .iter()
        .filter(|s| app.show_ignored || !app.ignored.contains(&s.session_id))
        .count();
    let mode_str = match app.view_mode {
        ViewMode::Date => "by last activity",
        ViewMode::DirectoryTree => "folder tree",
    };
    let indexing = if app.index.is_indexing() {
        format!(
            "  indexing {}/{}",
            app.index.done.load(Ordering::Relaxed),
            app.index.total.load(Ordering::Relaxed)
        )
    } else {
        String::new()
    };

    let (block, line) = match app.input_mode {
        InputMode::Move => (
            bordered_block("move session to folder"),
            Line::from(vec![
                Span::raw(app.move_buffer.as_str()),
                Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            ]),
        ),
        InputMode::Rename => (
            bordered_block("rename"),
            Line::from(vec![
                Span::raw(app.rename_buffer.as_str()),
                Span::styled("_", Style::default().add_modifier(Modifier::SLOW_BLINK)),
            ]),
        ),
        InputMode::Search | InputMode::ConfirmDelete | InputMode::ConfirmOpen(_) => (
            bordered_block(&format!(
                "Agent Session Finder  {count}/{total} [{mode_str}]{indexing}"
            )),
            Line::from(vec![
                Span::styled("> ", Style::default().fg(ACCENT)),
                Span::raw(app.search_query.as_str()),
                Span::styled(
                    "_",
                    Style::default()
                        .fg(ACCENT)
                        .add_modifier(Modifier::SLOW_BLINK),
                ),
            ]),
        ),
    };

    frame.render_widget(Paragraph::new(line).block(block), area);
}

fn draw_session_list(frame: &mut ratatui::Frame, area: Rect, app: &mut App) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(ACCENT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    let list_height = inner.height as usize;
    if list_height == 0 {
        return;
    }

    // Adjust scroll to keep selected visible
    if app.selected < app.scroll_offset {
        app.scroll_offset = app.selected;
    } else if app.selected >= app.scroll_offset + list_height {
        app.scroll_offset = app.selected - list_height + 1;
    }

    let visible_range =
        app.scroll_offset..std::cmp::min(app.scroll_offset + list_height, app.filtered_items.len());

    let mut lines: Vec<Line> = Vec::new();

    for (idx, i) in (visible_range.start..).zip(app.filtered_items[visible_range.clone()].iter()) {
        let is_selected = idx == app.selected;
        let line = match i {
            ListItem::DirectoryHeader {
                name,
                depth,
                expanded,
                ..
            } => {
                let indicator = if *expanded { "▼" } else { "▸" };
                let indent = "  ".repeat(*depth);
                let mut spans = vec![
                    Span::styled(
                        format!(" {indent}{indicator} "),
                        Style::default().fg(ACCENT),
                    ),
                    Span::styled(name.clone(), Style::default().add_modifier(Modifier::BOLD)),
                ];
                if is_selected {
                    for span in &mut spans {
                        span.style = span.style.bg(Color::Rgb(60, 60, 60)).fg(Color::White);
                    }
                    spans[1].style = spans[1].style.fg(ACCENT).add_modifier(Modifier::BOLD);
                }
                Line::from(highlight_spans(spans, &[1], &app.search_query))
            }
            ListItem::Session(s, depth) => {
                let live = app.live.get(&s.session_id).copied();
                let stats = app
                    .index
                    .doc(&s.session_id)
                    .map(|d| (d.question_count, d.line_count));
                let line = render_session_line(
                    s,
                    is_selected,
                    app.view_mode,
                    *depth,
                    live,
                    stats,
                    inner.width as usize,
                    &app.search_query,
                );
                if app.ignored.contains(&s.session_id) {
                    dim_ignored(line)
                } else {
                    line
                }
            }
        };
        lines.push(line);
    }

    while lines.len() < list_height {
        lines.push(Line::raw(""));
    }

    frame.render_widget(Paragraph::new(lines), inner);
}

#[allow(clippy::too_many_arguments)]
fn render_session_line(
    s: &SessionMeta,
    is_selected: bool,
    view_mode: ViewMode,
    depth: usize,
    live: Option<LiveStatus>,
    // (questions, lines) from the text index; None while not indexed yet
    stats: Option<(usize, usize)>,
    // Width of the list panel, for right-aligning the branch
    width: usize,
    // Search text to highlight in the name and path
    query: &str,
) -> Line<'static> {
    let ts = format_timestamp(s.last_modified);
    let name = s.session_name.as_deref().unwrap_or("(unnamed)").to_string();
    let stats = match stats {
        Some((q, l)) => format!("[{q}q/{}l]", format_k(l as u64)),
        None => "[\u{2026}]".to_string(),
    };

    let tree = view_mode == ViewMode::DirectoryTree;
    let indent = if tree {
        format!(" {}  ", "  ".repeat(depth))
    } else {
        " ".to_string()
    };

    let dim = Style::default().fg(Color::DarkGray);
    let normal = Style::default();

    let mut spans = vec![
        Span::styled(indent, normal),
        Span::styled(ts, dim),
        // Open in a running Claude process: filled dot = busy, hollow = idle.
        live_marker(live),
        Span::styled(name, normal),
    ];

    spans.push(Span::styled("  ", normal));
    spans.push(Span::styled(stats, dim));

    if view_mode == ViewMode::Date {
        spans.push(Span::styled("  ", normal));
        spans.push(Span::styled(tilde_path(&s.cwd), dim));
    }

    if is_selected {
        for span in &mut spans {
            span.style = span.style.bg(Color::Rgb(60, 60, 60)).fg(Color::White);
        }
        // Make the session name stand out
        if spans.len() > 3 {
            spans[3].style = spans[3].style.add_modifier(Modifier::BOLD);
        }
        spans[2] = live_marker(live).patch_style(Style::default().bg(Color::Rgb(60, 60, 60)));
        // Pointer indicator
        spans[0] = Span::styled(
            if tree {
                format!(" {}▶ ", "  ".repeat(depth))
            } else {
                "▶".to_string()
            },
            Style::default().fg(ACCENT).bg(Color::Rgb(60, 60, 60)),
        );
    }

    // Search matches in the name (index 3) and, in the date view, the path.
    let searched: &[usize] = if view_mode == ViewMode::Date {
        &[3, 7]
    } else {
        &[3]
    };
    let spans = highlight_spans(spans, searched, query);

    // Branch at the right edge of the panel. If the row is too long, the
    // left part is cut (with "…") so the branch stays visible.
    let bg = if is_selected {
        Style::default().bg(Color::Rgb(60, 60, 60))
    } else {
        Style::default()
    };
    let Some(branch) = &s.git_branch else {
        return Line::from(spans);
    };
    let branch = Span::styled(
        format!("\u{2387} {branch}"),
        Style::default().fg(BRANCH_COLOR).patch(bg),
    );
    let room = width.saturating_sub(branch.width() + 2);
    let mut left = Line::from(spans);
    if left.width() > room {
        left = truncate_line(left, room.saturating_sub(1));
        left.push_span(Span::styled("\u{2026}", DIM.patch(bg)));
    }
    let pad = width.saturating_sub(left.width() + branch.width());
    left.push_span(Span::styled(" ".repeat(pad), bg));
    left.push_span(branch);
    left
}

/// Highlight occurrences of `query` inside the spans at the given indices.
fn highlight_spans(spans: Vec<Span<'static>>, idxs: &[usize], query: &str) -> Vec<Span<'static>> {
    if query.is_empty() {
        return spans;
    }
    spans
        .into_iter()
        .enumerate()
        .flat_map(|(i, span)| {
            if idxs.contains(&i) {
                highlight(&span.content, 0, query, span.style, None)
            } else {
                vec![span]
            }
        })
        .collect()
}

/// Grey italic rendering for sessions matched by the ignore file. Search
/// highlights keep their colors.
fn dim_ignored(line: Line<'static>) -> Line<'static> {
    let spans = line
        .spans
        .into_iter()
        .map(|mut span| {
            if span.style.bg != Some(ACCENT) {
                span.style = span.style.fg(Color::DarkGray);
            }
            span.style = span.style.add_modifier(Modifier::ITALIC);
            span
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

/// Keep at most `max` display columns of a line.
fn truncate_line(line: Line<'static>, max: usize) -> Line<'static> {
    let mut out = Vec::new();
    let mut used = 0;
    for span in line.spans {
        let w = span.width();
        if used + w <= max {
            used += w;
            out.push(span);
            continue;
        }
        let mut text = String::new();
        for c in span.content.chars() {
            let cw = Span::raw(c.to_string()).width();
            if used + cw > max {
                break;
            }
            used += cw;
            text.push(c);
        }
        out.push(Span::styled(text, span.style));
        break;
    }
    Line::from(out)
}

const BRANCH_COLOR: Color = Color::Rgb(130, 170, 120);

fn live_marker(live: Option<LiveStatus>) -> Span<'static> {
    let accent = Style::default().fg(ACCENT);
    match live {
        Some(LiveStatus::Busy) => Span::styled(" \u{25cf} ", accent),
        Some(LiveStatus::Idle) => Span::styled(" \u{25cb} ", accent),
        None => Span::raw("   "),
    }
}

fn draw_preview(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let session = app.selected_session();
    let hits = session.and_then(|s| app.content_hits(&s.session_id));
    let current = hits.map(|h| app.match_index.min(h.hits.len().saturating_sub(1)));

    // While searching, the match counter goes into the title.
    let title = match (hits, current) {
        (Some(h), Some(i)) => {
            let capped = if h.count > h.hits.len() {
                format!(", first {} reachable", h.hits.len())
            } else {
                String::new()
            };
            format!("preview  match {}/{}{capped}", i + 1, h.count)
        }
        _ => "preview".to_string(),
    };
    let block = bordered_block(&title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let dim = Style::default().fg(Color::DarkGray);
    let placeholder = |frame: &mut ratatui::Frame, msg: &'static str| {
        frame.render_widget(Paragraph::new(Span::styled(msg, dim)), inner);
    };

    let Some(session) = session else {
        return placeholder(frame, "no session selected");
    };
    let Some(doc) = app.index.doc(&session.session_id) else {
        return placeholder(frame, "indexing...");
    };
    if inner.height < 3 {
        return;
    }

    let query = app.content_query();
    let focus = hits.zip(current).and_then(|(h, i)| h.hits.get(i)).copied();

    // Searching: the conversation uses the whole pane, centered on the
    // current match. Otherwise: question list on top, conversation below.
    let conv_area = if focus.is_some() {
        inner
    } else {
        let max_top = (inner.height as usize - 1) / 2;
        let top_lines = question_lines(&doc, max_top, inner.width as usize);
        let chunks = Layout::vertical([
            Constraint::Length(top_lines.len() as u16),
            Constraint::Length(1),
            Constraint::Fill(1),
        ])
        .split(inner);
        frame.render_widget(Paragraph::new(top_lines), chunks[0]);
        let sep = "\u{2500}".repeat(inner.width as usize);
        frame.render_widget(Paragraph::new(Span::styled(sep, dim)), chunks[1]);
        chunks[2]
    };

    app.preview_height.set(conv_area.height as usize);
    match hits.zip(current) {
        Some((h, i)) if focus.is_some() => draw_search_conversation(
            frame,
            conv_area,
            app,
            &doc,
            &session.session_id,
            h,
            i,
            query,
        ),
        _ => draw_conversation(frame, conv_area, &doc, query, &app.preview_scroll),
    }
}

const QUESTION_STYLE: Style = Style::new().fg(Color::Cyan);
const ASSISTANT_STYLE: Style = Style::new().fg(Color::White);
const DIM: Style = Style::new().fg(Color::DarkGray);
/// The match Ctrl-n/Ctrl-p currently points at.
const CURRENT_MATCH: Style = Style::new()
    .fg(Color::Black)
    .bg(Color::Green)
    .add_modifier(Modifier::BOLD);
const HIGHLIGHT: Style = Style::new()
    .fg(Color::Black)
    .bg(ACCENT)
    .add_modifier(Modifier::BOLD);

/// User questions in order, each wrapped to at most QUESTION_LIST_ROWS rows.
/// If they don't fit in `max` rows, keep the first and the most recent ones
/// with a "... N more" line in between.
fn question_lines(doc: &Doc, max: usize, width: usize) -> Vec<Line<'static>> {
    const QUESTION_LIST_ROWS: usize = 3;

    // Room for the 2-column bullet/indent and a trailing "…".
    let wrap_width = width.saturating_sub(3).max(1);
    let blocks: Vec<Vec<Line<'static>>> = doc
        .questions()
        .map(|q| {
            let flat = flatten(q.trim());
            let rows = wrap_rows(&flat, 0, flat.len(), wrap_width);
            let shown = rows.len().min(QUESTION_LIST_ROWS);
            rows[..shown]
                .iter()
                .enumerate()
                .map(|(ri, &(rs, re))| {
                    let lead = if ri == 0 { "\u{2022} " } else { "  " };
                    let mut spans = vec![
                        Span::raw(lead),
                        Span::styled(flat[rs..re].to_string(), QUESTION_STYLE),
                    ];
                    if ri + 1 == shown && shown < rows.len() {
                        spans.push(Span::styled("\u{2026}", DIM));
                    }
                    Line::from(spans)
                })
                .collect()
        })
        .collect();

    let total: usize = blocks.iter().map(Vec::len).sum();
    if total <= max {
        return blocks.into_iter().flatten().collect();
    }

    // One row for the "... N more" line; split the rest between the first
    // and the most recent questions.
    let budget = max.saturating_sub(1);
    let mut head_rows = 0;
    let mut head = 0;
    while head < blocks.len() && head_rows + blocks[head].len() <= budget / 2 {
        head_rows += blocks[head].len();
        head += 1;
    }
    let mut tail_rows = 0;
    let mut tail = 0;
    while tail < blocks.len() - head
        && head_rows + tail_rows + blocks[blocks.len() - 1 - tail].len() <= budget
    {
        tail_rows += blocks[blocks.len() - 1 - tail].len();
        tail += 1;
    }
    let skipped = blocks.len() - head - tail;
    let n = blocks.len();

    let mut lines: Vec<Line<'static>> = blocks[..head].iter().flatten().cloned().collect();
    lines.push(Line::styled(format!("  ... {skipped} more"), DIM));
    lines.extend(blocks[n - tail..].iter().flatten().cloned());
    lines
}

/// A match count line plus one excerpt line per match (as many as fit).
/// Lower part of the preview. Without a focused match it shows the end of
/// the conversation; with one, it centers the match (clamped at the start
/// and end of the conversation).
fn draw_conversation(
    frame: &mut ratatui::Frame,
    area: Rect,
    doc: &Doc,
    query: &str,
    manual_scroll: &std::cell::Cell<isize>,
) {
    if area.height == 0 || area.width == 0 || doc.msgs.is_empty() {
        return;
    }
    let n = doc.msgs.len();
    let width = area.width;
    let height = area.height as usize;

    // The last messages. If the manual scroll goes past the loaded messages,
    // load twice as many and try again.
    let mut ctx = 30;
    loop {
        let msg_range = n.saturating_sub(ctx)..n;
        let (lines, _) =
            conversation_lines(doc, msg_range.clone(), &|_| false, query, None, width, &[]);
        let total = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(width);
        let max_scroll = total.saturating_sub(height);
        let want = max_scroll as isize + manual_scroll.get();
        if want < 0 && msg_range.start > 0 && ctx < n {
            ctx *= 2;
            continue;
        }
        let scroll = want.clamp(0, max_scroll as isize);
        // Don't let the offset run past the ends, so scrolling back is immediate.
        manual_scroll.set(scroll - max_scroll as isize);
        let para = Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((u16::try_from(scroll).unwrap_or(u16::MAX), 0));
        frame.render_widget(para, area);
        return;
    }
}

type LayoutKey = (String, String, u16, usize, usize, usize);

/// The whole conversation laid out for the search view.
struct SearchLayout {
    lines: Vec<Line<'static>>,
    /// Wrapped row where each logical line starts (plus the total at the end)
    row_starts: Vec<usize>,
    /// Wrapped row of each search hit
    hit_rows: Vec<usize>,
}

impl SearchLayout {
    fn build(doc: &Doc, hits: &SessionHits, current: usize, query: &str, width: u16) -> Self {
        let full: std::collections::HashSet<usize> = hits.hits.iter().map(|h| h.msg).collect();
        let anchors: Vec<usize> = hits.hits.iter().map(|h| h.range.0).collect();
        let (lines, pos) = conversation_lines(
            doc,
            0..doc.msgs.len(),
            &|i| full.contains(&i),
            query,
            hits.hits.get(current).map(|h| h.range.0),
            width,
            &anchors,
        );
        let mut row_starts = Vec::with_capacity(lines.len() + 1);
        let mut row = 0;
        for l in &lines {
            row_starts.push(row);
            row += Paragraph::new(l.clone())
                .wrap(Wrap { trim: false })
                .line_count(width)
                .max(1);
        }
        row_starts.push(row);
        let hit_rows = pos
            .iter()
            .map(|&(li, col)| row_starts[li] + col / width.max(1) as usize)
            .collect();
        Self {
            lines,
            row_starts,
            hit_rows,
        }
    }
}

/// Search view: the whole conversation with a stable layout. Moving to
/// another match only scrolls, and only if that match is off-screen.
#[allow(clippy::too_many_arguments)]
fn draw_search_conversation(
    frame: &mut ratatui::Frame,
    area: Rect,
    app: &App,
    doc: &Arc<Doc>,
    session_id: &str,
    hits: &SessionHits,
    current: usize,
    query: &str,
) {
    if area.height == 0 || area.width == 0 || doc.msgs.is_empty() {
        return;
    }
    let height = area.height as usize;
    let key: LayoutKey = (
        session_id.to_string(),
        query.to_string(),
        area.width,
        Arc::as_ptr(doc) as usize,
        doc.msgs.len(),
        current,
    );
    let mut cache = app.search_layout.borrow_mut();
    if cache.as_ref().is_none_or(|(k, _)| *k != key) {
        *cache = Some((
            key,
            SearchLayout::build(doc, hits, current, query, area.width),
        ));
    }
    let layout = &cache.as_ref().expect("just built").1;

    let total = *layout.row_starts.last().unwrap_or(&0);
    let max_scroll = total.saturating_sub(height);
    let row = layout.hit_rows.get(current).copied().unwrap_or(0);
    let centered = row.saturating_sub(height / 2);
    let mut top = app.search_scroll.get().unwrap_or(centered);
    // Re-center only if the new match is off-screen or closer than MARGIN
    // rows to the top or bottom edge.
    const MARGIN: usize = 5;
    let margin = MARGIN.min(height.saturating_sub(1) / 2);
    if app.reveal_match.take() && (row < top + margin || row + margin >= top + height) {
        top = centered;
    }
    let top = top.min(max_scroll);
    app.search_scroll.set(Some(top));

    // Render only the logical lines that reach into the visible rows.
    let n = layout.lines.len();
    let first = layout.row_starts[..n]
        .partition_point(|&r| r <= top)
        .saturating_sub(1);
    let end = layout.row_starts[..n]
        .partition_point(|&r| r < top + height)
        .max(first + 1);
    let offset = top - layout.row_starts[first];
    let para = Paragraph::new(layout.lines[first..end.min(n)].to_vec())
        .wrap(Wrap { trim: false })
        .scroll((u16::try_from(offset).unwrap_or(u16::MAX), 0));
    frame.render_widget(para, area);
}

/// Lines for the messages in `msg_range`, plus the position of the focused
/// match as (logical line index, column).
/// Lines for the messages in `msg_range`. Messages for which `full` is true
/// are shown completely, others are cut (3000 bytes, questions 5 rows).
/// `current` is the offset of the match to mark as current. For each offset
/// in `anchors` (sorted), returns its position as (logical line, column).
fn conversation_lines(
    doc: &Doc,
    msg_range: std::ops::Range<usize>,
    full: &dyn Fn(usize) -> bool,
    query: &str,
    current: Option<usize>,
    width: u16,
    anchors: &[usize],
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    const MAX_MSG_BYTES: usize = 3000;
    const MAX_QUESTION_ROWS: usize = 5;

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut pos_out = Vec::with_capacity(anchors.len());
    // Record anchors that start before `end` as lying on the next line.
    let mut place = |lines_len: usize, start: usize, end: usize, lead: usize| {
        while pos_out.len() < anchors.len() && anchors[pos_out.len()] < end {
            let a = anchors[pos_out.len()];
            let col = if a >= start {
                doc.text[start..a].chars().count() + lead
            } else {
                0
            };
            pos_out.push((lines_len, col));
        }
    };

    for i in msg_range {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        let msg = &doc.msgs[i];
        let (prefix, style) = role_style(msg.role, "> ", "");
        let show_all = full(i);
        let lo = msg.range.start;
        let hi = if show_all {
            msg.range.end
        } else {
            floor_char(&doc.text, lo + MAX_MSG_BYTES).min(msg.range.end)
        };
        let mut cut_back = hi < msg.range.end;

        if msg.role == Role::User {
            // Wrap ourselves, leaving room for the 2-column prefix and a
            // trailing "…" so ratatui never re-wraps.
            let wrap_width = (width as usize).saturating_sub(3).max(1);
            let rows = wrap_rows(&doc.text, lo, hi, wrap_width);
            let last = if show_all {
                rows.len()
            } else {
                rows.len().min(MAX_QUESTION_ROWS)
            };
            cut_back = cut_back || last < rows.len();

            for (ri, &(rs, re)) in rows[..last].iter().enumerate() {
                let lead = if ri == 0 { prefix } else { "  " };
                place(lines.len(), rs, re, lead.len());
                let mut spans = vec![Span::styled(lead, DIM)];
                spans.extend(highlight(&doc.text[rs..re], rs, query, style, current));
                lines.push(Line::from(spans));
            }
        } else {
            let mut pos = lo;
            for (li, text) in doc.text[lo..hi].split('\n').enumerate() {
                let lead = if li == 0 { prefix } else { "" };
                place(lines.len(), pos, pos + text.len(), lead.len());
                let mut spans = vec![Span::styled(lead, DIM)];
                spans.extend(highlight(text, pos, query, style, current));
                lines.push(Line::from(spans));
                pos += text.len() + 1;
            }
        }
        if cut_back {
            if let Some(last) = lines.last_mut() {
                last.push_span(Span::styled("\u{2026}", DIM));
            }
        }
    }
    let last_line = lines.len().saturating_sub(1);
    while pos_out.len() < anchors.len() {
        pos_out.push((last_line, 0));
    }
    (lines, pos_out)
}

/// Word-wrap `text[lo..hi]` into rows of at most `width` chars.
/// Returns absolute byte ranges into `text`; newlines always start a new row.
fn wrap_rows(text: &str, lo: usize, hi: usize, width: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    let mut line_start = lo;
    for line in text[lo..hi].split('\n') {
        let line_end = line_start + line.len();
        let mut start = line_start;
        loop {
            let rest = &text[start..line_end];
            // Byte offset (relative to `start`) just past `width` chars.
            let Some((cut, next)) = rest.char_indices().nth(width) else {
                rows.push((start, line_end));
                break;
            };
            if next == ' ' {
                // The row ends exactly at a word boundary.
                rows.push((start, start + cut));
                start += cut + 1;
                continue;
            }
            match rest[..cut].rfind(' ') {
                Some(sp) if sp > 0 => {
                    rows.push((start, start + sp));
                    start += sp + 1;
                }
                _ => {
                    rows.push((start, start + cut));
                    start += cut;
                }
            }
        }
        line_start = line_end + 1;
    }
    rows
}

fn role_style(role: Role, user: &'static str, assistant: &'static str) -> (&'static str, Style) {
    match role {
        Role::User => (user, QUESTION_STYLE),
        Role::Assistant => (assistant, ASSISTANT_STYLE),
    }
}

/// Split `text` into spans, highlighting every ASCII-case-insensitive
/// occurrence of `query`.
/// `base` is the byte offset of `text` in `Doc::text`; the occurrence starting
/// at `current` (also an offset in `Doc::text`) gets the CURRENT_MATCH style.
fn highlight(
    text: &str,
    base: usize,
    query: &str,
    style: Style,
    current: Option<usize>,
) -> Vec<Span<'static>> {
    if query.is_empty() {
        return vec![Span::styled(text.to_string(), style)];
    }
    // ASCII lowercasing keeps byte offsets identical.
    let hay = text.to_ascii_lowercase();
    let needle = query.to_ascii_lowercase();
    let mut spans = Vec::new();
    let mut last = 0;
    for (start, _) in hay.match_indices(&needle) {
        if start < last {
            continue;
        }
        if start > last {
            spans.push(Span::styled(text[last..start].to_string(), style));
        }
        let end = start + needle.len();
        let hl = if current == Some(base + start) {
            CURRENT_MATCH
        } else {
            HIGHLIGHT
        };
        spans.push(Span::styled(text[start..end].to_string(), hl));
        last = end;
    }
    if last < text.len() {
        spans.push(Span::styled(text[last..].to_string(), style));
    }
    spans
}

/// Collapse each whitespace run (including newlines) into a single space.
/// Leading and trailing whitespace is kept as one space, so excerpts glued
/// together around a match keep their word boundaries.
fn flatten(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_ws = false;
    for c in text.chars() {
        if c.is_whitespace() {
            if !in_ws {
                out.push(' ');
            }
            in_ws = true;
        } else {
            out.push(c);
            in_ws = false;
        }
    }
    out
}

fn floor_char(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn draw_footer(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let dim = Style::default().fg(Color::DarkGray);
    let key_style = Style::default().fg(ACCENT);
    let sep = Span::styled(" \u{2502} ", dim); // │ separator

    let shortcuts = match app.input_mode {
        InputMode::ConfirmDelete | InputMode::ConfirmOpen(_) => vec![
            Span::styled("\u{2190}/\u{2192}", key_style),
            Span::styled(" select", dim),
            sep.clone(),
            Span::styled("Enter", key_style),
            Span::styled(" confirm", dim),
            sep.clone(),
            Span::styled("y", key_style),
            Span::styled("/", dim),
            Span::styled("n", key_style),
            Span::styled(" answer", dim),
            sep.clone(),
            Span::styled("Esc", key_style),
            Span::styled(" cancel", dim),
        ],
        InputMode::Move => vec![
            Span::styled("Tab", key_style),
            Span::styled(" complete", dim),
            sep.clone(),
            Span::styled("Enter", key_style),
            Span::styled(" move", dim),
            sep.clone(),
            Span::styled("Esc", key_style),
            Span::styled(" cancel", dim),
        ],
        InputMode::Rename => vec![
            Span::styled("Enter", key_style),
            Span::styled(" confirm", dim),
            sep.clone(),
            Span::styled("Esc", key_style),
            Span::styled(" cancel", dim),
        ],
        _ => {
            let mut v = vec![
                Span::styled("Enter", key_style),
                Span::styled("/", dim),
                Span::styled("Alt-a", key_style),
                Span::styled(format!(" {}", app.open_label), dim),
                sep.clone(),
            ];
            if let Some(alt) = &app.alt_label {
                v.extend([
                    Span::styled("Alt-v", key_style),
                    Span::styled(format!(" {alt}"), dim),
                    sep.clone(),
                ]);
            }
            v.extend([
                Span::styled("C-j/C-k", key_style),
                Span::styled(" move", dim),
                sep.clone(),
                Span::styled("C-n/C-p", key_style),
                Span::styled(" match", dim),
                sep.clone(),
                Span::styled("C-\u{2191}/\u{2193}", key_style),
                Span::styled(" scroll", dim),
                sep.clone(),
                Span::styled("\u{2190}/\u{2192}", key_style),
                Span::styled(" fold", dim),
                sep.clone(),
                Span::styled("Tab", key_style),
                Span::styled(" view", dim),
                sep.clone(),
                Span::styled("C-r", key_style),
                Span::styled(" rename", dim),
                sep.clone(),
                Span::styled("Alt-m", key_style),
                Span::styled(" move", dim),
                sep.clone(),
                Span::styled("Alt-h", key_style),
                Span::styled(" ignored", dim),
                sep.clone(),
                Span::styled("C-d", key_style),
                Span::styled(" delete", dim),
                sep.clone(),
                Span::styled("Esc", key_style),
                Span::styled(" quit", dim),
            ]);
            v
        }
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(ACCENT));

    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some((msg, is_error)) = &app.status {
        let color = if *is_error {
            Color::Yellow
        } else {
            Color::Green
        };
        frame.render_widget(
            Paragraph::new(Span::styled(msg.clone(), Style::default().fg(color))),
            inner,
        );
        return;
    }

    let shortcuts = Line::from(shortcuts);
    let legend = if app.input_mode == InputMode::Search {
        Line::from(vec![
            Span::styled("\u{25cf}", key_style),
            Span::styled(" busy  ", dim),
            Span::styled("\u{25cb}", key_style),
            Span::styled(" idle  ", dim),
            Span::styled("[q/l]", dim),
            Span::styled(" questions/lines", dim),
        ])
    } else {
        Line::raw("")
    };

    // Legend on the right, only if it fits next to the shortcuts.
    let needed = shortcuts.width() + 2 + legend.width();
    if legend.width() > 0 && needed <= inner.width as usize {
        let cols = Layout::horizontal([
            Constraint::Fill(1),
            Constraint::Length(legend.width() as u16),
        ])
        .split(inner);
        frame.render_widget(Paragraph::new(shortcuts), cols[0]);
        frame.render_widget(Paragraph::new(legend), cols[1]);
    } else {
        frame.render_widget(Paragraph::new(shortcuts), inner);
    }
}

fn format_timestamp(epoch_secs: u64) -> String {
    // Apply local timezone offset
    let local_secs = epoch_secs as i64 + local_utc_offset_secs();
    let days = local_secs.div_euclid(86400);
    let time_of_day = local_secs.rem_euclid(86400);
    let hours = time_of_day / 3600;
    let minutes = (time_of_day % 3600) / 60;

    let (year, month, day) = days_to_ymd(days);

    format!("{year:04}-{month:02}-{day:02} {hours:02}:{minutes:02}")
}

/// Get local timezone offset from UTC in seconds.
/// Uses libc localtime_r to get the system timezone.
fn local_utc_offset_secs() -> i64 {
    use std::sync::OnceLock;
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        // Use a known epoch time to determine the offset
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        #[cfg(unix)]
        {
            let mut tm: libc::tm = unsafe { std::mem::zeroed() };
            let time_t = now as libc::time_t;
            unsafe { libc::localtime_r(&time_t, &mut tm) };
            tm.tm_gmtoff
        }
        #[cfg(not(unix))]
        {
            0 // Fall back to UTC on non-Unix
        }
    })
}

fn days_to_ymd(days_since_epoch: i64) -> (i64, i64, i64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days_since_epoch + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1461 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

fn format_k(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{}K", n / 1000)
    } else {
        n.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(cwd: &str, t: u64) -> SessionMeta {
        SessionMeta {
            session_id: format!("{cwd}-{t}"),
            file_path: String::new(),
            cwd: cwd.into(),
            session_name: None,
            first_user_message: None,
            last_modified: t,
            git_branch: None,
            file_mtime: t,
            file_len: 0,
        }
    }

    fn rows(items: &[ListItem]) -> Vec<String> {
        items
            .iter()
            .map(|i| match i {
                ListItem::DirectoryHeader { name, depth, .. } => {
                    format!("{}{name}", "  ".repeat(*depth))
                }
                ListItem::Session(s, depth) => {
                    format!("{}* {}", "  ".repeat(*depth), s.last_modified)
                }
            })
            .collect()
    }

    #[test]
    fn tilde_path_shortens_home() {
        let h = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        assert_eq!(tilde_path(&h), "~");
        assert_eq!(tilde_path(&format!("{h}/asf")), "~/asf");
        assert_eq!(tilde_path(&format!("{h}x/asf")), format!("{h}x/asf"));
        assert_eq!(tilde_path("/tmp/foo"), "/tmp/foo");
    }

    #[test]
    fn tree_groups_and_compacts() {
        let h = dirs::home_dir().unwrap().to_string_lossy().into_owned();
        let all = [
            session(&format!("{h}/abc"), 1),
            session(&format!("{h}/def/a"), 3),
            session(&format!("{h}/def/b"), 2),
            session(&format!("{h}/x/y/z"), 0),
            session("/tmp/foo", 4),
        ];
        let refs: Vec<&SessionMeta> = all.iter().collect();
        assert_eq!(
            rows(&build_tree(&refs, None)),
            [
                "/tmp/foo/",
                "  * 4",
                "~/def/",
                "  a/",
                "    * 3",
                "  b/",
                "    * 2",
                "~/abc/",
                "  * 1",
                "~/x/y/z/",
                "  * 0",
            ]
        );

        // A session directly in ~ makes ~ a row: sessions first, then folders.
        let home_session = session(&h, 5);
        let mut refs2 = refs.clone();
        refs2.push(&home_session);
        let r = rows(&build_tree(&refs2, None));
        assert_eq!(
            r,
            [
                "~/",
                "  * 5",
                "  def/",
                "    a/",
                "      * 3",
                "    b/",
                "      * 2",
                "  abc/",
                "    * 1",
                "  x/y/z/",
                "    * 0",
                "/tmp/foo/",
                "  * 4",
            ]
        );

        // Collapsing hides the folder contents.
        let collapsed = std::collections::HashSet::from([format!("{h}/def")]);
        let r = rows(&build_tree(&refs, Some(&collapsed)));
        assert_eq!(&r[2..4], ["~/def/", "~/abc/"]);
    }

    #[test]
    fn wrap_rows_breaks_at_spaces_and_newlines() {
        let t = "aaa bbb ccc\ndddddddd";
        let rows: Vec<&str> = wrap_rows(t, 0, t.len(), 7)
            .into_iter()
            .map(|(s, e)| &t[s..e])
            .collect();
        assert_eq!(rows, ["aaa bbb", "ccc", "ddddddd", "d"]);
        let u = "ä ö ü";
        assert_eq!(wrap_rows(u, 0, u.len(), 3).len(), 2);
    }

    #[test]
    fn flatten_keeps_word_boundaries() {
        assert_eq!(flatten("please "), "please ");
        assert_eq!(flatten(" tool.\n\nthere"), " tool. there");
    }

    #[test]
    fn highlight_is_case_insensitive() {
        let spans = highlight(
            "Use TMPDIR or tmpdir",
            0,
            "tmpdir",
            Style::default(),
            Some(14),
        );
        let hl: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == HIGHLIGHT)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(hl, ["TMPDIR"]);
        let cur: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == CURRENT_MATCH)
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(cur, ["tmpdir"]);
    }
}
