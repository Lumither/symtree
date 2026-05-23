use std::{
    cell::{Ref, RefCell},
    collections::{HashMap, HashSet},
    env, mem,
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

/// Run `$body` and return `(result, elapsed)`. With `debug_perf` off the timing
/// path compiles away to just the body and a `Duration::ZERO`.
#[cfg(feature = "debug_perf")]
#[macro_export]
macro_rules! timed {
    ($($body:tt)*) => {{
        let __t = ::std::time::Instant::now();
        let __result = { $($body)* };
        (__result, __t.elapsed())
    }};
}

#[cfg(not(feature = "debug_perf"))]
#[macro_export]
macro_rules! timed {
    ($($body:tt)*) => {{
        let __result = { $($body)* };
        (__result, ::std::time::Duration::ZERO)
    }};
}

mod autocomplete;
mod preview;
mod render;
mod watcher;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{DefaultTerminal, widgets::ListState};

use self::preview::{LOADING_INDICATOR_DELAY, PreviewCache, PreviewRequest, preview_worker};
use self::render::render;
use self::watcher::{SYMBOL_RELOAD_DEBOUNCE, fs_event_should_trigger_reload, loader_worker};

use crate::{
    error::{AppContext, AppResult},
    languages::LanguageDef,
    lsp::LoadEvent,
    model::ProjectSymbols,
    query::{self, Expr},
    tree::{
        SelectionTarget, VisibleNode, flatten_visible, get_node, get_node_mut, selection_target,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphMode {
    Unicode,
    Ascii,
}

/// Close-on-Esc/Enter/q + scroll keys (j/k/d/u/g/G/PageUp/Down/Home/End) for
/// help / warnings / lsp / perf overlays.
macro_rules! overlay_close {
    ($self:ident, $key:ident, $flag:ident) => {
        if $self.$flag {
            match $key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    $self.$flag = false;
                    $self.overlay_scroll = 0;
                    $self.message.clear();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    $self.overlay_scroll = $self.overlay_scroll.saturating_add(1);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    $self.overlay_scroll = $self.overlay_scroll.saturating_sub(1);
                }
                KeyCode::Char('d') | KeyCode::PageDown => {
                    $self.overlay_scroll = $self.overlay_scroll.saturating_add(10);
                }
                KeyCode::Char('u') | KeyCode::PageUp => {
                    $self.overlay_scroll = $self.overlay_scroll.saturating_sub(10);
                }
                KeyCode::Char('g') | KeyCode::Home => {
                    $self.overlay_scroll = 0;
                }
                KeyCode::Char('G') | KeyCode::End => {
                    $self.overlay_scroll = usize::MAX;
                }
                _ => {}
            }
            return Ok(Action::None);
        }
    };
}

/// Open an overlay by setting its flag, resetting scroll, and clearing the
/// status bus.
macro_rules! overlay_open {
    ($self:ident, $flag:ident) => {{
        $self.$flag = true;
        $self.overlay_scroll = 0;
        $self.message.clear();
    }};
}

fn char_index_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

fn insert_at_cursor(buf: &mut String, cursor: &mut usize, ch: char) {
    let byte = char_index_to_byte(buf, *cursor);
    buf.insert(byte, ch);
    *cursor += 1;
}

fn delete_before_cursor(buf: &mut String, cursor: &mut usize) -> bool {
    if *cursor == 0 {
        return false;
    }
    let byte = char_index_to_byte(buf, *cursor - 1);
    buf.remove(byte);
    *cursor -= 1;
    true
}

fn delete_at_cursor(buf: &mut String, cursor: usize) -> bool {
    let chars_count = buf.chars().count();
    if cursor >= chars_count {
        return false;
    }
    let byte = char_index_to_byte(buf, cursor);
    buf.remove(byte);
    true
}

fn prev_word(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    if cursor == 0 {
        return 0;
    }
    let mut i = cursor.min(chars.len());
    while i > 0 && chars[i - 1].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    i
}

fn next_word(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    let len = chars.len();
    let mut i = cursor.min(len);
    while i < len && !chars[i].is_whitespace() {
        i += 1;
    }
    while i < len && chars[i].is_whitespace() {
        i += 1;
    }
    i
}

fn set_expanded_recursive(nodes: &mut [crate::model::SymbolNode], expanded: bool) {
    for node in nodes {
        node.expanded = expanded;
        set_expanded_recursive(&mut node.children, expanded);
    }
}

fn default_expanded(node: &crate::model::SymbolNode) -> bool {
    !matches!(node.kind, crate::model::SymbolKind::File)
}

fn collect_expanded_overrides(nodes: &[crate::model::SymbolNode]) -> HashMap<Vec<String>, bool> {
    fn walk(
        nodes: &[crate::model::SymbolNode],
        path: &mut Vec<String>,
        out: &mut HashMap<Vec<String>, bool>,
    ) {
        for node in nodes {
            path.push(node.name.clone());
            if node.expanded != default_expanded(node) {
                out.insert(path.clone(), node.expanded);
            }
            walk(&node.children, path, out);
            path.pop();
        }
    }
    let mut out = HashMap::new();
    let mut path = Vec::new();
    walk(nodes, &mut path, &mut out);
    out
}

fn apply_expanded_overrides(
    node: &mut crate::model::SymbolNode,
    overrides: &HashMap<Vec<String>, bool>,
) {
    fn walk(
        node: &mut crate::model::SymbolNode,
        path: &mut Vec<String>,
        overrides: &HashMap<Vec<String>, bool>,
    ) {
        path.push(node.name.clone());
        if let Some(&v) = overrides.get(path) {
            node.expanded = v;
        }
        for child in &mut node.children {
            walk(child, path, overrides);
        }
        path.pop();
    }
    let mut path = Vec::new();
    walk(node, &mut path, overrides);
}

fn name_path_for(
    nodes: &[crate::model::SymbolNode],
    index_path: &[usize],
) -> Option<Vec<String>> {
    let mut out = Vec::with_capacity(index_path.len());
    let mut current = nodes;
    for &idx in index_path {
        let node = current.get(idx)?;
        out.push(node.name.clone());
        current = &node.children;
    }
    Some(out)
}

fn name_path_to_index_path(
    nodes: &[crate::model::SymbolNode],
    name_path: &[String],
) -> Option<Vec<usize>> {
    let mut out = Vec::with_capacity(name_path.len());
    let mut current = nodes;
    for name in name_path {
        let i = current.iter().position(|n| &n.name == name)?;
        out.push(i);
        current = &current[i].children;
    }
    Some(out)
}

fn compose_load_message(prefix: &str, warnings: &[String]) -> String {
    match warnings.len() {
        0 => prefix.to_string(),
        1 => format!("{prefix} — ! {}", warnings[0]),
        n => format!("{prefix} — ! {} (+{} more)", warnings[0], n - 1),
    }
}

pub fn run(
    root: PathBuf,
    languages: Vec<LanguageDef>,
    glyph_mode: GlyphMode,
    symbols: ProjectSymbols,
) -> AppResult<()> {
    let mut terminal = ratatui::try_init().context("failed to initialize terminal")?;
    let result = run_app(
        &mut terminal,
        App::new(root, languages, glyph_mode, symbols),
    );
    ratatui::restore();
    result
}

struct App {
    root: PathBuf,
    languages: Vec<LanguageDef>,
    project: ProjectSymbols,
    selected: usize,
    list_state: ListState,
    mode: Mode,
    filter: String,
    command: String,
    message: String,
    glyph_mode: GlyphMode,
    show_help: bool,
    show_keymap: bool,
    show_warnings: bool,
    show_lsp: bool,
    #[cfg(feature = "debug_perf")]
    show_perf: bool,
    should_quit: bool,
    preview_cache: Option<PreviewCache>,
    preview_request_tx: Sender<PreviewRequest>,
    preview_result_rx: Receiver<PreviewCache>,
    preview_in_flight: Option<(PathBuf, Instant)>,
    preview_last_line: usize,
    loading_overlay_shown: bool,
    fs_event_rx: Receiver<notify::Result<notify::Event>>,
    _fs_watcher: Option<RecommendedWatcher>,
    symbols_dirty_at: Option<Instant>,
    load_request_tx: Sender<()>,
    load_event_rx: Receiver<LoadEvent>,
    load_in_flight: bool,
    loaded_file_count: usize,
    discovered_file_count: usize,
    watched_files: HashSet<PathBuf>,
    center_selection_pending: bool,
    visible_cache: RefCell<Option<Vec<VisibleNode>>>,
    scroll_offset: usize,
    perf: RefCell<PerfStats>,
    symbol_count_cache: usize,
    query: Option<Expr>,
    query_source: Option<String>,
    candidate_index: usize,
    cursor_pos: usize,
    overlay_scroll: usize,
    pending_expanded: HashMap<Vec<String>, bool>,
    pending_selection_name_path: Option<Vec<String>>,
    pending_selection_row: usize,
}

#[cfg(feature = "debug_perf")]
mod perf {
    use std::time::Duration;

    #[derive(Default, Debug, Clone)]
    pub(crate) struct PerfStats {
        pub frame_count: u64,
        pub total_frame: Duration,
        pub last_frame: Duration,
        pub max_frame: Duration,
        pub last_tree: Duration,
        pub last_details: Duration,
        pub last_preview: Duration,
        pub last_footer: Duration,
        pub flatten_calls: u64,
        pub flatten_total: Duration,
        pub flatten_last: Duration,
        pub flatten_max: Duration,
        pub flatten_last_size: usize,
        pub load_events_drained: u64,
        pub last_drain_size: usize,
    }

    impl PerfStats {
        pub(crate) fn record_frame(&mut self, dur: Duration) {
            self.frame_count += 1;
            self.total_frame = self.total_frame.saturating_add(dur);
            self.last_frame = dur;
            if dur > self.max_frame {
                self.max_frame = dur;
            }
        }

        pub(crate) fn record_flatten(&mut self, dur: Duration, size: usize) {
            self.flatten_calls += 1;
            self.flatten_total = self.flatten_total.saturating_add(dur);
            self.flatten_last = dur;
            self.flatten_last_size = size;
            if dur > self.flatten_max {
                self.flatten_max = dur;
            }
        }

        pub(crate) fn record_render_tree(&mut self, dur: Duration) {
            self.last_tree = dur;
        }
        pub(crate) fn record_render_details(&mut self, dur: Duration) {
            self.last_details = dur;
        }
        pub(crate) fn record_render_preview(&mut self, dur: Duration) {
            self.last_preview = dur;
        }
        pub(crate) fn record_render_footer(&mut self, dur: Duration) {
            self.last_footer = dur;
        }
        pub(crate) fn record_load_drain(&mut self, count: usize) {
            self.load_events_drained = self.load_events_drained.saturating_add(count as u64);
            self.last_drain_size = count;
        }

        pub fn avg_frame(&self) -> Duration {
            if self.frame_count == 0 {
                Duration::ZERO
            } else {
                Duration::from_nanos(
                    (self.total_frame.as_nanos() / self.frame_count as u128) as u64,
                )
            }
        }

        pub fn avg_flatten(&self) -> Duration {
            if self.flatten_calls == 0 {
                Duration::ZERO
            } else {
                Duration::from_nanos(
                    (self.flatten_total.as_nanos() / self.flatten_calls as u128) as u64,
                )
            }
        }
    }
}

#[cfg(not(feature = "debug_perf"))]
mod perf {
    use std::time::Duration;

    #[derive(Default, Debug, Clone)]
    pub(crate) struct PerfStats;

    impl PerfStats {
        #[inline]
        pub(crate) fn record_frame(&mut self, _: Duration) {}
        #[inline]
        pub(crate) fn record_flatten(&mut self, _: Duration, _: usize) {}
        #[inline]
        pub(crate) fn record_render_tree(&mut self, _: Duration) {}
        #[inline]
        pub(crate) fn record_render_details(&mut self, _: Duration) {}
        #[inline]
        pub(crate) fn record_render_preview(&mut self, _: Duration) {}
        #[inline]
        pub(crate) fn record_render_footer(&mut self, _: Duration) {}
        #[inline]
        pub(crate) fn record_load_drain(&mut self, _: usize) {}
    }
}

pub(crate) use perf::PerfStats;

impl App {
    fn new(
        root: PathBuf,
        languages: Vec<LanguageDef>,
        glyph_mode: GlyphMode,
        project: ProjectSymbols,
    ) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        let (result_tx, result_rx) = mpsc::channel::<PreviewCache>();
        thread::spawn(move || preview_worker(request_rx, result_tx));

        let (load_request_tx, load_request_rx) = mpsc::channel::<()>();
        let (load_event_tx, load_event_rx) = mpsc::channel::<LoadEvent>();
        {
            let root = root.clone();
            let langs = languages.clone();
            thread::spawn(move || loader_worker(root, langs, load_request_rx, load_event_tx));
        }

        let (fs_event_tx, fs_event_rx) = mpsc::channel::<notify::Result<notify::Event>>();
        let fs_watcher = (|| -> Option<RecommendedWatcher> {
            let mut watcher = notify::recommended_watcher(move |res| {
                let _ = fs_event_tx.send(res);
            })
            .ok()?;
            watcher.watch(&root, RecursiveMode::Recursive).ok()?;
            Some(watcher)
        })();

        let mut app = Self {
            root,
            languages,
            project,
            selected: 0,
            list_state: ListState::default(),
            mode: Mode::Normal,
            filter: String::new(),
            command: String::new(),
            message: String::new(),
            glyph_mode,
            show_help: false,
            show_keymap: false,
            show_warnings: false,
            show_lsp: false,
            #[cfg(feature = "debug_perf")]
            show_perf: false,
            should_quit: false,
            preview_cache: None,
            preview_request_tx: request_tx,
            preview_result_rx: result_rx,
            preview_in_flight: None,
            preview_last_line: 1,
            loading_overlay_shown: false,
            fs_event_rx,
            _fs_watcher: fs_watcher,
            symbols_dirty_at: None,
            load_request_tx,
            load_event_rx,
            load_in_flight: false,
            loaded_file_count: 0,
            discovered_file_count: 0,
            watched_files: HashSet::new(),
            center_selection_pending: false,
            visible_cache: RefCell::new(None),
            scroll_offset: 0,
            perf: RefCell::new(<PerfStats as Default>::default()),
            symbol_count_cache: 0,
            query: None,
            query_source: None,
            candidate_index: 0,
            cursor_pos: 0,
            overlay_scroll: 0,
            pending_expanded: HashMap::new(),
            pending_selection_name_path: None,
            pending_selection_row: 0,
        };
        app.symbol_count_cache = app.project.symbol_count();
        app.refresh_watched_files();
        if app.project.files.is_empty() {
            let _ = app.load_request_tx.send(());
            app.load_in_flight = true;
        } else if !app.project.warnings.is_empty() {
            app.set_load_message("Loaded");
        }
        app
    }

    fn set_load_message(&mut self, prefix: &str) {
        self.message = compose_load_message(prefix, &self.project.warnings);
    }

    fn request_reload(&mut self) {
        if self.load_in_flight {
            return;
        }
        let _ = self.load_request_tx.send(());
        self.load_in_flight = true;
    }

    fn handle_load_event(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Started => {
                let previous_index_path = self.selected_path();
                self.pending_selection_name_path = previous_index_path
                    .as_deref()
                    .and_then(|p| name_path_for(&self.project.files, p));
                self.pending_selection_row = self.selected;
                self.pending_expanded = collect_expanded_overrides(&self.project.files);

                self.project.files.clear();
                self.project.warnings.clear();
                self.invalidate_visible();
                self.symbol_count_cache = 0;
                self.loaded_file_count = 0;
                self.discovered_file_count = 0;
                self.load_in_flight = true;
                self.preview_cache = None;
                self.preview_in_flight = None;
                self.loading_overlay_shown = false;
            }
            LoadEvent::Discovered(n) => {
                self.discovered_file_count = self.discovered_file_count.saturating_add(n);
            }
            LoadEvent::FileLoaded(mut file) => {
                if !self.pending_expanded.is_empty() {
                    apply_expanded_overrides(&mut file, &self.pending_expanded);
                }
                self.symbol_count_cache = self
                    .symbol_count_cache
                    .saturating_add(file.descendant_count());
                self.project.files.push(file);
                self.invalidate_visible();
                self.loaded_file_count += 1;
            }
            LoadEvent::Warning(text) => {
                self.message = format!("! {text}");
                self.project.warnings.push(text);
            }
            LoadEvent::Finished => {
                self.project.files.sort_by(|a, b| a.name.cmp(&b.name));
                self.invalidate_visible();
                self.load_in_flight = false;
                self.refresh_watched_files();

                let restored_index_path = self
                    .pending_selection_name_path
                    .as_deref()
                    .and_then(|np| name_path_to_index_path(&self.project.files, np));
                self.restore_selection(
                    restored_index_path.as_deref(),
                    self.pending_selection_row,
                );
                self.pending_selection_name_path = None;
                self.pending_selection_row = 0;
                self.pending_expanded.clear();

                self.set_load_message(&format!("Loaded {} files", self.loaded_file_count));
            }
        }
    }

    fn refresh_watched_files(&mut self) {
        self.watched_files = self
            .project
            .files
            .iter()
            .map(|node| self.root.join(&node.name))
            .collect();
    }

    fn visible_nodes(&self) -> Ref<'_, Vec<VisibleNode>> {
        if self.visible_cache.borrow().is_none() {
            let start = Instant::now();
            let query = self.active_query();
            let computed = flatten_visible(&self.project.files, query.as_ref());
            let elapsed = start.elapsed();
            let size = computed.len();
            *self.visible_cache.borrow_mut() = Some(computed);
            self.perf.borrow_mut().record_flatten(elapsed, size);
        }
        Ref::map(self.visible_cache.borrow(), |opt| {
            opt.as_ref().expect("filled above")
        })
    }

    fn invalidate_visible(&self) {
        *self.visible_cache.borrow_mut() = None;
    }

    fn active_query(&self) -> Option<Expr> {
        let from_filter = if self.filter.trim().is_empty() {
            None
        } else {
            query::parse(&self.filter).ok().flatten()
        };
        match (self.query.clone(), from_filter) {
            (Some(a), Some(b)) => Some(Expr::And(vec![a, b])),
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        }
    }

    fn clamp_selection(&mut self) {
        let len = self.visible_nodes().len();
        if len == 0 {
            self.selected = 0;
            self.list_state.select(None);
        } else {
            self.selected = self.selected.min(len - 1);
            self.list_state.select(Some(self.selected));
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> AppResult<Action> {
        if key.kind == KeyEventKind::Release {
            return Ok(Action::None);
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key),
            Mode::Search => Ok(self.handle_search_key(key)),
            Mode::Command => Ok(self.handle_command_key(key)),
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> AppResult<Action> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Ok(Action::Quit);
        }

        overlay_close!(self, key, show_help);
        overlay_close!(self, key, show_keymap);
        overlay_close!(self, key, show_warnings);
        overlay_close!(self, key, show_lsp);
        #[cfg(feature = "debug_perf")]
        overlay_close!(self, key, show_perf);

        let action = match key.code {
            KeyCode::Char('q') => Action::Quit,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    let previous_path = self.selected_path();
                    let previous_row = self.selected;
                    self.filter.clear();
                    self.invalidate_visible();
                    self.restore_selection(previous_path.as_deref(), previous_row);
                    self.center_selection_pending = true;
                    self.message = "Filter cleared".to_string();
                }
                Action::None
            }
            KeyCode::Char('K') => {
                self.move_by(-3);
                Action::None
            }
            KeyCode::Char('J') => {
                self.move_by(3);
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_up();
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_down();
                Action::None
            }
            KeyCode::PageUp => {
                self.page_up();
                Action::None
            }
            KeyCode::PageDown => {
                self.page_down();
                Action::None
            }
            KeyCode::Home => {
                self.move_home();
                Action::None
            }
            KeyCode::End => {
                self.move_end();
                Action::None
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.toggle_selected();
                Action::None
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.move_to_parent();
                Action::None
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.move_to_first_child();
                Action::None
            }
            KeyCode::Char('u') => {
                self.move_to_next_sibling();
                Action::None
            }
            KeyCode::Char('i') => {
                self.move_to_previous_sibling();
                Action::None
            }
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.cursor_pos = self.filter.chars().count();
                self.candidate_index = 0;
                self.message.clear();
                Action::None
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.cursor_pos = 0;
                self.candidate_index = 0;
                self.message.clear();
                Action::None
            }
            KeyCode::Char('r') => Action::Reload,
            KeyCode::Char('o') => Action::OpenSelection,
            _ => Action::None,
        };

        Ok(action)
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.clear();
                self.cursor_pos = 0;
                self.selected = 0;
                self.candidate_index = 0;
                self.invalidate_visible();
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = 0;
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = self.filter.chars().count();
                Action::None
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = prev_word(&self.filter, self.cursor_pos);
                Action::None
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = next_word(&self.filter, self.cursor_pos);
                Action::None
            }
            KeyCode::Esc | KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Tab => {
                let cands = autocomplete::search_candidates(self);
                if let Some(pick) =
                    cands.get(self.candidate_index.min(cands.len().saturating_sub(1)))
                {
                    self.filter = autocomplete::replace_last_token(&self.filter, pick);
                    self.cursor_pos = self.filter.chars().count();
                    self.invalidate_visible();
                }
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Up => {
                let cands = autocomplete::search_candidates(self);
                let len = cands.len();
                if len > 0 {
                    self.candidate_index = (self.candidate_index + len - 1) % len;
                }
                Action::None
            }
            KeyCode::Down => {
                let cands = autocomplete::search_candidates(self);
                let len = cands.len();
                if len > 0 {
                    self.candidate_index = (self.candidate_index + 1) % len;
                }
                Action::None
            }
            KeyCode::Left => {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                self.cursor_pos = (self.cursor_pos + 1).min(self.filter.chars().count());
                Action::None
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                Action::None
            }
            KeyCode::End => {
                self.cursor_pos = self.filter.chars().count();
                Action::None
            }
            KeyCode::Backspace => {
                if delete_before_cursor(&mut self.filter, &mut self.cursor_pos) {
                    self.selected = 0;
                    self.candidate_index = 0;
                    self.invalidate_visible();
                }
                Action::None
            }
            KeyCode::Delete => {
                if delete_at_cursor(&mut self.filter, self.cursor_pos) {
                    self.selected = 0;
                    self.candidate_index = 0;
                    self.invalidate_visible();
                }
                Action::None
            }
            KeyCode::Char(char) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    insert_at_cursor(&mut self.filter, &mut self.cursor_pos, char);
                    self.selected = 0;
                    self.candidate_index = 0;
                    self.invalidate_visible();
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.command.clear();
                self.cursor_pos = 0;
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = 0;
                Action::None
            }
            KeyCode::Char('e') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = self.command.chars().count();
                Action::None
            }
            KeyCode::Left if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = prev_word(&self.command, self.cursor_pos);
                Action::None
            }
            KeyCode::Right if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.cursor_pos = next_word(&self.command, self.cursor_pos);
                Action::None
            }
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Enter => {
                if self.command.trim() == "query"
                    && let Some(src) = self.query_source.clone()
                {
                    self.command = format!("query {src}");
                    self.cursor_pos = self.command.chars().count();
                    self.candidate_index = 0;
                    return Action::None;
                }
                self.execute_command();
                self.mode = Mode::Normal;
                self.command.clear();
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Tab => {
                let cands = autocomplete::command_candidates(self);
                if let Some(pick) =
                    cands.get(self.candidate_index.min(cands.len().saturating_sub(1)))
                {
                    self.command = autocomplete::replace_last_token(&self.command, pick);
                    self.cursor_pos = self.command.chars().count();
                }
                self.candidate_index = 0;
                Action::None
            }
            KeyCode::Up => {
                let cands = autocomplete::command_candidates(self);
                let len = cands.len();
                if len > 0 {
                    self.candidate_index = (self.candidate_index + len - 1) % len;
                }
                Action::None
            }
            KeyCode::Down => {
                let cands = autocomplete::command_candidates(self);
                let len = cands.len();
                if len > 0 {
                    self.candidate_index = (self.candidate_index + 1) % len;
                }
                Action::None
            }
            KeyCode::Left => {
                self.cursor_pos = self.cursor_pos.saturating_sub(1);
                Action::None
            }
            KeyCode::Right => {
                self.cursor_pos = (self.cursor_pos + 1).min(self.command.chars().count());
                Action::None
            }
            KeyCode::Home => {
                self.cursor_pos = 0;
                Action::None
            }
            KeyCode::End => {
                self.cursor_pos = self.command.chars().count();
                Action::None
            }
            KeyCode::Backspace => {
                if delete_before_cursor(&mut self.command, &mut self.cursor_pos) {
                    self.candidate_index = 0;
                }
                Action::None
            }
            KeyCode::Delete => {
                if delete_at_cursor(&mut self.command, self.cursor_pos) {
                    self.candidate_index = 0;
                }
                Action::None
            }
            KeyCode::Char(char) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    insert_at_cursor(&mut self.command, &mut self.cursor_pos, char);
                    self.candidate_index = 0;
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    fn execute_command(&mut self) {
        let command = self.command.trim().to_string();
        if command.is_empty() {
            self.message.clear();
            return;
        }

        match command.as_str() {
            "help" => {
                self.show_help = true;
                self.overlay_scroll = 0;
                self.message = "Help".to_string();
            }
            "keymap" | "keys" => {
                self.show_keymap = true;
                self.overlay_scroll = 0;
                self.message = "Keymap".to_string();
            }
            "warnings" | "w" => {
                if self.project.warnings.is_empty() {
                    self.message = "No warnings".to_string();
                } else {
                    self.show_warnings = true;
                    self.overlay_scroll = 0;
                    self.message.clear();
                }
            }
            "lsp" => overlay_open!(self, show_lsp),
            #[cfg(feature = "debug_perf")]
            "perf" => overlay_open!(self, show_perf),
            #[cfg(feature = "debug_perf")]
            "perf reset" => {
                *self.perf.borrow_mut() = PerfStats::default();
                self.message = "perf stats cleared".to_string();
            }
            "collapse" => {
                set_expanded_recursive(&mut self.project.files, false);
                self.invalidate_visible();
                self.selected = 0;
                self.message = "Collapsed all".to_string();
            }
            "expand" => {
                set_expanded_recursive(&mut self.project.files, true);
                self.invalidate_visible();
                self.message = "Expanded all".to_string();
            }
            "q" | "quit" => {
                self.should_quit = true;
            }
            "query" | "query clear" => {
                self.query = None;
                self.query_source = None;
                self.invalidate_visible();
                self.selected = 0;
                self.message = "query cleared".to_string();
            }
            _ if command.starts_with("query ") => {
                let expr_src = command["query ".len()..].trim();
                if expr_src.is_empty() || expr_src == "clear" {
                    self.query = None;
                    self.query_source = None;
                    self.invalidate_visible();
                    self.selected = 0;
                    self.message = "query cleared".to_string();
                } else {
                    match query::parse(expr_src) {
                        Ok(Some(expr)) => {
                            self.query = Some(expr);
                            self.query_source = Some(expr_src.to_string());
                            self.invalidate_visible();
                            self.selected = 0;
                            self.message.clear();
                        }
                        Ok(None) => {
                            self.query = None;
                            self.query_source = None;
                            self.invalidate_visible();
                            self.message = "query cleared".to_string();
                        }
                        Err(err) => {
                            self.message = format!("! query: {err}");
                        }
                    }
                }
            }
            _ => {
                self.message = format!("Unknown command: :{command}");
            }
        }
    }

    fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    fn move_down(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = (self.selected + 1).min(len - 1);
        }
    }

    fn move_by(&mut self, delta: isize) {
        let len = self.visible_nodes().len();
        if len == 0 {
            self.selected = 0;
            return;
        }

        let next = self
            .selected
            .saturating_add_signed(delta)
            .min(len.saturating_sub(1));
        self.selected = next;
    }

    fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(10);
    }

    fn page_down(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = (self.selected + 10).min(len - 1);
        }
    }

    fn move_home(&mut self) {
        self.selected = 0;
    }

    fn move_end(&mut self) {
        let len = self.visible_nodes().len();
        if len > 0 {
            self.selected = len - 1;
        }
    }

    fn toggle_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        if let Some(node) = get_node_mut(&mut self.project.files, &path)
            && node.has_children()
        {
            node.expanded = !node.expanded;
            self.invalidate_visible();
        }
    }

    fn move_to_parent(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };

        if path.len() <= 1 {
            return;
        }

        let parent = path[..path.len() - 1].to_vec();
        let index = self
            .visible_nodes()
            .iter()
            .position(|candidate| candidate.path == parent);
        if let Some(index) = index {
            self.selected = index;
        }
    }

    fn move_to_first_child(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };

        if let Some(node) = get_node_mut(&mut self.project.files, &path)
            && node.has_children()
        {
            node.expanded = true;
            self.invalidate_visible();
        }

        let index = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let child_depth = current.depth + 1;
            visible
                .iter()
                .enumerate()
                .skip(self.selected + 1)
                .take_while(|(_, node)| node.depth > current.depth)
                .find_map(|(index, node)| (node.depth == child_depth).then_some(index))
        };
        if let Some(index) = index {
            self.selected = index;
        }
    }

    fn move_to_previous_sibling(&mut self) {
        let index = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let current_path = current.path.clone();
            visible[..self.selected]
                .iter()
                .rposition(|node| has_same_parent(&node.path, &current_path))
        };
        if let Some(index) = index {
            self.selected = index;
        }
    }

    fn move_to_next_sibling(&mut self) {
        let offset = {
            let visible = self.visible_nodes();
            let Some(current) = visible.get(self.selected) else {
                return;
            };
            let current_path = current.path.clone();
            visible[self.selected + 1..]
                .iter()
                .position(|node| has_same_parent(&node.path, &current_path))
        };
        if let Some(offset) = offset {
            self.selected += offset + 1;
        }
    }

    fn selected_path(&self) -> Option<Vec<usize>> {
        self.visible_nodes()
            .get(self.selected)
            .map(|node| node.path.clone())
    }

    fn selected_node(&self) -> Option<&crate::model::SymbolNode> {
        let path = self.selected_path()?;
        get_node(&self.project.files, &path)
    }

    fn selected_target(&self) -> Option<SelectionTarget> {
        let path = self.selected_path()?;
        selection_target(&self.root, &self.project.files, &path)
    }

    fn restore_selection(&mut self, previous_path: Option<&[usize]>, previous_row: usize) {
        enum Restored {
            Empty,
            Index(usize),
            Fallback(usize),
        }
        let action = {
            let visible = self.visible_nodes();
            if visible.is_empty() {
                Restored::Empty
            } else if let Some(path) = previous_path
                && let Some(index) = visible.iter().position(|node| node.path.as_slice() == path)
            {
                Restored::Index(index)
            } else if let Some(path) = previous_path {
                let mut found = None;
                for len in (1..path.len()).rev() {
                    let ancestor = &path[..len];
                    if let Some(index) = visible
                        .iter()
                        .position(|node| node.path.as_slice() == ancestor)
                    {
                        found = Some(index);
                        break;
                    }
                }
                match found {
                    Some(i) => Restored::Index(i),
                    None => Restored::Fallback(previous_row.min(visible.len() - 1)),
                }
            } else {
                Restored::Fallback(previous_row.min(visible.len() - 1))
            }
        };
        match action {
            Restored::Empty => {
                self.selected = 0;
                self.list_state = ListState::default();
            }
            Restored::Index(i) | Restored::Fallback(i) => self.selected = i,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    Command,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    Reload,
    OpenSelection,
}

fn run_app(terminal: &mut DefaultTerminal, mut app: App) -> AppResult<()> {
    let mut needs_draw = true;

    while !app.should_quit {
        let mut got_fs_event = false;
        while let Ok(event) = app.fs_event_rx.try_recv() {
            if matches!(event, Ok(ref ev) if fs_event_should_trigger_reload(ev, &app.watched_files))
            {
                got_fs_event = true;
            }
        }
        if got_fs_event {
            app.symbols_dirty_at = Some(Instant::now());
        }

        if let Some(t) = app.symbols_dirty_at
            && t.elapsed() >= SYMBOL_RELOAD_DEBOUNCE
            && !app.load_in_flight
        {
            app.symbols_dirty_at = None;
            app.request_reload();
            needs_draw = true;
        }

        let mut drained = 0usize;
        while let Ok(event) = app.load_event_rx.try_recv() {
            drained += 1;
            app.handle_load_event(event);
        }
        if drained > 0 {
            app.perf.borrow_mut().record_load_drain(drained);
            needs_draw = true;
        }

        let current_target = app.selected_target().map(|t| t.file);
        let mut got_result = false;
        while let Ok(cache) = app.preview_result_rx.try_recv() {
            got_result = true;
            if app
                .preview_in_flight
                .as_ref()
                .is_some_and(|(p, _)| *p == cache.path)
            {
                app.preview_in_flight = None;
                app.loading_overlay_shown = false;
            }
            if current_target.as_ref() == Some(&cache.path) {
                app.preview_cache = Some(cache);
            }
        }
        if got_result {
            needs_draw = true;
        }

        let overlay_due = app
            .preview_in_flight
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() >= LOADING_INDICATOR_DELAY);
        if overlay_due && !app.loading_overlay_shown {
            needs_draw = true;
        }

        if needs_draw {
            app.clamp_selection();
            let frame_start = Instant::now();
            terminal
                .draw(|frame| render(frame, &mut app))
                .context("failed to render frame")?;
            let elapsed = frame_start.elapsed();
            app.perf.borrow_mut().record_frame(elapsed);
            needs_draw = false;
        }

        let timeout = next_poll_timeout(&app);
        if !event::poll(timeout).context("failed to poll terminal events")? {
            continue;
        }

        match event::read().context("failed to read terminal event")? {
            Event::Key(key) => {
                match app.handle_key(key)? {
                    Action::None => {}
                    Action::Quit => app.should_quit = true,
                    Action::Reload => app.request_reload(),
                    Action::OpenSelection => open_selection(terminal, &mut app)?,
                }
                needs_draw = true;
            }
            Event::Resize(_, _) => {
                needs_draw = true;
            }
            _ => {}
        }
    }

    Ok(())
}

fn open_selection(terminal: &mut DefaultTerminal, app: &mut App) -> AppResult<()> {
    let Some(target) = app.selected_target() else {
        app.message = "Nothing selected".to_string();
        return Ok(());
    };

    ratatui::try_restore().context("failed to restore terminal before opening editor")?;
    let launch_result = launch_editor(&target);
    let restored = ratatui::try_init().context("failed to restore TUI after editor")?;
    let _ = mem::replace(terminal, restored);

    match launch_result {
        Ok(status) if status.success() => {
            app.message = format!(
                "Opened {}:{} ({})",
                target.file.display(),
                target.line,
                target.label
            );
        }
        Ok(status) => {
            app.message = format!("Editor exited with status {status}");
        }
        Err(error) => {
            app.message = format!("Failed to open editor: {error}");
        }
    }

    Ok(())
}

fn launch_editor(target: &SelectionTarget) -> AppResult<std::process::ExitStatus> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let mut command = Command::new(program);
    command.args(parts);
    command.arg(format!("+{}", target.line));
    command.arg(&target.file);
    command
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))
}

fn next_poll_timeout(app: &App) -> Duration {
    let mut deadline = Duration::from_millis(120);
    if let Some((_, t)) = app.preview_in_flight.as_ref() {
        let elapsed = t.elapsed();
        let wait = if elapsed < LOADING_INDICATOR_DELAY {
            (LOADING_INDICATOR_DELAY - elapsed).max(Duration::from_millis(1))
        } else {
            Duration::from_millis(15)
        };
        deadline = deadline.min(wait);
    }
    if let Some(t) = app.symbols_dirty_at {
        let elapsed = t.elapsed();
        if elapsed < SYMBOL_RELOAD_DEBOUNCE {
            deadline =
                deadline.min((SYMBOL_RELOAD_DEBOUNCE - elapsed).max(Duration::from_millis(1)));
        }
    }
    if app.load_in_flight {
        deadline = deadline.min(Duration::from_millis(30));
    }
    deadline
}

fn has_same_parent(left: &[usize], right: &[usize]) -> bool {
    left.len() == right.len()
        && left.split_last().map(|(_, parent)| parent).unwrap_or(&[])
            == right.split_last().map(|(_, parent)| parent).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::render::{Glyphs, match_range, status_line};
    use super::*;
    use crate::model::{SymbolKind, SymbolNode};
    use ratatui::text::Line;
    use std::path::PathBuf;

    #[test]
    fn match_range_finds_ascii_case_insensitive_substring() {
        assert_eq!(
            match_range("respond_to_server_request", "RESP"),
            Some((0, 4))
        );
        assert_eq!(match_range("is_response_for", "resp"), Some((3, 7)));
    }

    #[test]
    fn match_range_uses_utf8_safe_boundaries() {
        assert_eq!(match_range("handle_数据_resp", "resp"), Some((14, 18)));
    }

    #[test]
    fn match_range_ignores_empty_filter() {
        assert_eq!(match_range("anything", ""), None);
        assert_eq!(match_range("anything", "   "), None);
    }

    #[test]
    fn connector_does_not_continue_past_last_parent() {
        let glyphs = Glyphs::new(GlyphMode::Unicode);
        let visible = VisibleNode {
            path: vec![0, 3, 0],
            depth: 2,
            is_last: false,
            ancestor_is_last: vec![false, true],
            matched: false,
        };

        assert_eq!(glyphs.connector(&visible), "  ├─");
    }

    #[test]
    fn connector_continues_through_non_last_parent() {
        let glyphs = Glyphs::new(GlyphMode::Unicode);
        let visible = VisibleNode {
            path: vec![0, 2, 0],
            depth: 2,
            is_last: true,
            ancestor_is_last: vec![false, false],
            matched: false,
        };

        assert_eq!(glyphs.connector(&visible), "│ └─");
    }

    #[test]
    fn h_moves_to_visible_parent() {
        let mut app = sample_app();
        app.selected = 2;

        app.move_to_parent();

        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0][..]));
    }

    #[test]
    fn l_expands_and_moves_to_first_child() {
        let mut app = sample_app();
        app.project.files[0].children[0].expanded = false;
        app.selected = 1;

        app.move_to_first_child();

        assert!(app.project.files[0].children[0].expanded);
        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 0][..]));
    }

    #[test]
    fn u_i_move_between_visible_siblings() {
        let mut app = sample_app();
        app.selected = 2;

        app.move_to_next_sibling();
        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 1][..]));

        app.move_to_next_sibling();
        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 1][..]));

        app.move_to_previous_sibling();
        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 0][..]));

        app.move_to_previous_sibling();
        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 0][..]));
    }

    #[test]
    fn upper_j_k_move_three_rows() {
        let mut app = sample_app();

        app.move_by(3);
        assert_eq!(app.selected, 3);

        app.move_by(-3);
        assert_eq!(app.selected, 0);
    }

    #[test]
    fn restore_selection_keeps_same_path_after_reload() {
        let mut app = sample_app();
        app.selected = 3;
        let previous_path = app.selected_path();
        app.project = sample_app().project;

        app.restore_selection(previous_path.as_deref(), 3);

        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0, 1][..]));
    }

    #[test]
    fn restore_selection_falls_back_to_visible_ancestor() {
        let mut app = sample_app();
        let previous_path = vec![0, 0, 7];
        app.project.files[0].children[0].children.clear();

        app.restore_selection(Some(&previous_path), 3);

        assert_eq!(app.selected_path().as_deref(), Some(&[0, 0][..]));
    }

    #[test]
    fn reload_preserves_expansion_and_cursor() {
        let mut app = sample_app();
        set_expanded_recursive(&mut app.project.files, false);
        app.project.files[0].expanded = true;
        app.project.files[0].children[0].expanded = true;
        app.invalidate_visible();
        app.selected = 2;
        let target_path = app.selected_path().expect("selection");
        let target_names =
            name_path_for(&app.project.files, &target_path).expect("name path");

        let saved_files = app.project.files.clone();

        app.handle_load_event(LoadEvent::Started);
        for file in saved_files {
            app.handle_load_event(LoadEvent::FileLoaded(file));
        }
        app.handle_load_event(LoadEvent::Finished);

        assert!(app.project.files[0].expanded);
        assert!(app.project.files[0].children[0].expanded);
        assert!(!app.project.files[0].children[1].expanded);

        let restored_names =
            name_path_for(&app.project.files, &app.selected_path().expect("selection"))
                .expect("restored name path");
        assert_eq!(restored_names, target_names);
    }

    #[test]
    fn restore_selection_falls_back_to_nearest_row() {
        let mut app = sample_app();
        let previous_path = vec![9, 9];
        app.project.files.truncate(1);

        app.restore_selection(Some(&previous_path), 99);

        assert_eq!(app.selected, app.visible_nodes().len() - 1);
    }

    #[test]
    fn help_command_opens_help_overlay() {
        let mut app = sample_app();

        app.handle_key(key(KeyCode::Char(':'))).unwrap();
        for char in "help".chars() {
            app.handle_key(key(KeyCode::Char(char))).unwrap();
        }
        app.handle_key(key(KeyCode::Enter)).unwrap();

        assert_eq!(app.mode, Mode::Normal);
        assert!(app.show_help);
        assert!(app.command.is_empty());
    }

    #[test]
    fn unknown_command_sets_notification() {
        let mut app = sample_app();

        app.handle_key(key(KeyCode::Char(':'))).unwrap();
        for char in "missing".chars() {
            app.handle_key(key(KeyCode::Char(char))).unwrap();
        }
        app.handle_key(key(KeyCode::Enter)).unwrap();

        assert_eq!(app.message, "Unknown command: :missing");
        assert!(!app.show_help);
    }

    #[test]
    fn slash_filter_uses_prompt_mode() {
        let mut app = sample_app();

        app.handle_key(key(KeyCode::Char('/'))).unwrap();
        app.handle_key(key(KeyCode::Char('c'))).unwrap();
        app.handle_key(key(KeyCode::Char('h'))).unwrap();

        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.filter, "ch");
    }

    #[test]
    fn status_line_keeps_counts_on_the_right() {
        let app = sample_app();

        let text = line_text(&status_line(&app, 40));

        assert!(!text.contains("symtree"));
        assert!(text.starts_with("/workspace"));
        assert!(text.ends_with("files 2 symbols 5 visible 7"));
        assert_eq!(text.chars().count(), 40);
    }

    #[test]
    fn status_line_truncates_long_paths_from_the_left() {
        let mut app = sample_app();
        app.project.root = PathBuf::from("/very/long/path/that/will/not/fit");

        let text = line_text(&status_line(&app, 30));

        assert!(text.starts_with('~'));
        assert!(text.ends_with("files 2 symbols 5 visible 7"));
        assert_eq!(text.chars().count(), 30);
    }

    #[test]
    fn q_closes_help_before_quitting() {
        let mut app = sample_app();
        app.show_help = true;

        let action = app.handle_key(key(KeyCode::Char('q'))).unwrap();

        assert_eq!(action, Action::None);
        assert!(!app.show_help);
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn sample_app() -> App {
        App::new(
            PathBuf::from("/workspace"),
            vec![LanguageDef {
                lsp: "rust-analyzer".to_string(),
                extensions: vec!["rs".to_string()],
                language_id: Some("rust".to_string()),
            }],
            GlyphMode::Unicode,
            ProjectSymbols {
                root: PathBuf::from("/workspace"),
                files: vec![
                    {
                        let mut f = SymbolNode::file(
                            "src/lib.rs",
                            vec![
                                SymbolNode::new(
                                    "parent",
                                    SymbolKind::Lsp(2),
                                    Some(1),
                                    None,
                                    vec![
                                        SymbolNode::new(
                                            "child_a",
                                            SymbolKind::Lsp(12),
                                            Some(2),
                                            None,
                                            Vec::new(),
                                        ),
                                        SymbolNode::new(
                                            "child_b",
                                            SymbolKind::Lsp(12),
                                            Some(3),
                                            None,
                                            Vec::new(),
                                        ),
                                    ],
                                ),
                                SymbolNode::new(
                                    "other_parent",
                                    SymbolKind::Lsp(23),
                                    Some(5),
                                    None,
                                    vec![SymbolNode::new(
                                        "other_child",
                                        SymbolKind::Lsp(12),
                                        Some(6),
                                        None,
                                        Vec::new(),
                                    )],
                                ),
                            ],
                        );
                        f.expanded = true;
                        f
                    },
                    {
                        let mut f = SymbolNode::file("src/main.rs", Vec::new());
                        f.expanded = true;
                        f
                    },
                ],
                warnings: Vec::new(),
            },
        )
    }
}
