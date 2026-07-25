use std::{
    cell::{Ref, RefCell},
    collections::HashSet,
    path::PathBuf,
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
mod editor;
mod input;
mod load;
mod navigation;
mod preview;
mod render;
mod theme;
mod watcher;

use crossterm::event::{self, Event};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use ratatui::{DefaultTerminal, widgets::ListState};

use self::load::{DirtyState, PendingReload};
use self::preview::{LOADING_INDICATOR_DELAY, PreviewCache, PreviewRequest, preview_worker};
use self::render::render;
use self::watcher::{fs_event_should_trigger_reload, loader_worker};

use crate::{
    error::{AppContext, AppResult},
    languages::LanguageDef,
    lsp::LoadEvent,
    model::ProjectSymbols,
    query::{self, Expr},
    tree::{VisibleNode, flatten_visible},
};

/// The mutually-exclusive full-screen overlays. A single `Option<Overlay>`
/// replaces what used to be five separate `bool` flags, so at most one can be
/// open by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overlay {
    Help,
    Keymap,
    Warnings,
    Lsp,
    #[cfg(feature = "debug_perf")]
    Perf,
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

/// A single-line text input: the buffer plus a char-index cursor. Owns the
/// cursor-motion and editing primitives shared by the search (`/`) and command
/// (`:`) lines, so each key handler is just mode-specific glue. Derefs to `str`
/// so read-only callers can treat it like the string it wraps.
#[derive(Default)]
struct TextField {
    buf: String,
    cursor: usize,
}

impl TextField {
    fn as_str(&self) -> &str {
        &self.buf
    }

    fn char_len(&self) -> usize {
        self.buf.chars().count()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    /// Replace the contents and park the cursor at the end.
    fn set(&mut self, text: String) {
        self.buf = text;
        self.cursor = self.char_len();
    }

    fn cursor_to_start(&mut self) {
        self.cursor = 0;
    }

    fn cursor_to_end(&mut self) {
        self.cursor = self.char_len();
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_len());
    }

    fn word_left(&mut self) {
        self.cursor = prev_word(&self.buf, self.cursor);
    }

    fn word_right(&mut self) {
        self.cursor = next_word(&self.buf, self.cursor);
    }

    fn insert(&mut self, ch: char) {
        insert_at_cursor(&mut self.buf, &mut self.cursor, ch);
    }

    fn delete_before(&mut self) -> bool {
        delete_before_cursor(&mut self.buf, &mut self.cursor)
    }

    fn delete_at(&mut self) -> bool {
        delete_at_cursor(&mut self.buf, self.cursor)
    }
}

impl std::ops::Deref for TextField {
    type Target = str;
    fn deref(&self) -> &str {
        &self.buf
    }
}

pub fn run(root: PathBuf, languages: Vec<LanguageDef>, symbols: ProjectSymbols) -> AppResult<()> {
    let mut terminal = ratatui::try_init().context("failed to initialize terminal")?;
    let result = run_app(&mut terminal, App::new(root, languages, symbols));
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
    filter: TextField,
    command: TextField,
    message: String,
    overlay: Option<Overlay>,
    should_quit: bool,
    /// Set by the `o` key; consumed by the run loop, which holds the terminal
    /// handle needed to suspend the TUI while the editor runs.
    pending_open_selection: bool,
    preview_cache: Option<PreviewCache>,
    preview_request_tx: Sender<PreviewRequest>,
    preview_result_rx: Receiver<PreviewCache>,
    preview_in_flight: Option<(PathBuf, Instant)>,
    preview_last_line: usize,
    loading_overlay_shown: bool,
    fs_event_rx: Receiver<notify::Result<notify::Event>>,
    _fs_watcher: Option<RecommendedWatcher>,
    dirty: DirtyState,
    load_request_tx: Sender<()>,
    load_event_rx: Receiver<LoadEvent>,
    load_in_flight: bool,
    loaded_file_count: usize,
    discovered_file_count: usize,
    watched_files: HashSet<PathBuf>,
    source_extensions: HashSet<String>,
    center_selection_pending: bool,
    visible_cache: RefCell<Option<Vec<VisibleNode>>>,
    scroll_offset: usize,
    perf: RefCell<PerfStats>,
    symbol_count_cache: usize,
    query: Option<Expr>,
    query_source: Option<String>,
    candidate_index: usize,
    overlay_scroll: usize,
    pending_reload: Option<PendingReload>,
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

// Threading model
// ---------------
// `App` lives on the main thread (the render/event loop in `run_app`) and owns
// three detached worker threads, each reached over its own mpsc channel. None
// are joined; they exit when `App` is dropped and their `Sender`s/`Receiver`s
// disconnect.
//
//   * preview worker  — `preview_request_tx` → `preview_result_rx`
//       Reads + syntax-highlights the file under the cursor. Coalesces: keeps
//       only the latest request.
//   * loader thread   — `load_request_tx` → `load_event_rx`
//       Drives the LSP clients and streams `LoadEvent`s as symbols arrive.
//       Coalesces queued reload requests.
//   * fs watcher      — `notify` callback → `fs_event_rx`
//       Raw filesystem events, debounced by `DirtyState` into reloads.
impl App {
    fn new(root: PathBuf, languages: Vec<LanguageDef>, project: ProjectSymbols) -> Self {
        let (request_tx, request_rx) = mpsc::channel::<PreviewRequest>();
        let (result_tx, result_rx) = mpsc::channel::<PreviewCache>();
        thread::spawn(move || preview_worker(request_rx, result_tx));

        let source_extensions: HashSet<String> = languages
            .iter()
            .flat_map(|lang| lang.extensions.iter().cloned())
            .collect();

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
            filter: TextField::default(),
            command: TextField::default(),
            message: String::new(),
            overlay: None,
            should_quit: false,
            pending_open_selection: false,
            preview_cache: None,
            preview_request_tx: request_tx,
            preview_result_rx: result_rx,
            preview_in_flight: None,
            preview_last_line: 1,
            loading_overlay_shown: false,
            fs_event_rx,
            _fs_watcher: fs_watcher,
            dirty: DirtyState::default(),
            load_request_tx,
            load_event_rx,
            load_in_flight: false,
            loaded_file_count: 0,
            discovered_file_count: 0,
            watched_files: HashSet::new(),
            source_extensions,
            center_selection_pending: false,
            visible_cache: RefCell::new(None),
            scroll_offset: 0,
            perf: RefCell::new(<PerfStats as Default>::default()),
            symbol_count_cache: 0,
            query: None,
            query_source: None,
            candidate_index: 0,
            overlay_scroll: 0,
            pending_reload: None,
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
        query::combine(self.query.clone(), from_filter)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Normal,
    Search,
    Command,
}

fn run_app(terminal: &mut DefaultTerminal, mut app: App) -> AppResult<()> {
    let mut needs_draw = true;

    while !app.should_quit {
        let mut got_fs_event = false;
        while let Ok(event) = app.fs_event_rx.try_recv() {
            match event {
                Ok(ref ev)
                    if fs_event_should_trigger_reload(
                        ev,
                        &app.root,
                        &app.source_extensions,
                        &app.watched_files,
                    ) =>
                {
                    got_fs_event = true;
                }
                // A watcher error (e.g. an inotify queue overflow) means events
                // were dropped, so we may have missed real changes — resync.
                Err(_) => got_fs_event = true,
                _ => {}
            }
        }
        if got_fs_event {
            app.dirty.mark(Instant::now());
        }

        // Reload once the burst has settled (debounce) or we've waited the max.
        if app.dirty.ready() && !app.load_in_flight {
            app.dirty.clear();
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
                app.handle_key(key)?;
                if app.pending_open_selection {
                    app.pending_open_selection = false;
                    editor::open_selection(terminal, &mut app)?;
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
    if let Some(wait) = app.dirty.wake_in() {
        deadline = deadline.min(wait.max(Duration::from_millis(1)));
    }
    if app.load_in_flight {
        deadline = deadline.min(Duration::from_millis(30));
    }
    deadline
}

#[cfg(test)]
mod tests {
    use super::render::{Glyphs, display_width, match_range, status_line, truncate_left};
    use super::*;
    use crate::model::{SymbolKind, SymbolNode};
    use crate::tree::{name_path_for, set_expanded_recursive};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    fn display_width_counts_terminal_columns_not_scalars() {
        // Wide (CJK) glyphs occupy two columns each; the old chars().count()
        // would report 2 here instead of 4.
        assert_eq!(display_width("中文"), 4);
        assert_eq!(display_width("ab"), 2);
        // A zero-width combining mark adds no columns.
        assert_eq!(display_width("e\u{0301}"), 1);
    }

    #[test]
    fn truncate_left_respects_wide_glyph_columns() {
        // Each CJK glyph is two columns; truncating to 5 columns leaves the "~"
        // marker plus two full-width glyphs (4 columns), never a split glyph.
        let out = truncate_left("一二三四", 5);
        assert!(out.starts_with('~'));
        assert!(display_width(&out) <= 5);
        assert!(out.ends_with("三四"));
    }

    #[test]
    fn connector_does_not_continue_past_last_parent() {
        let glyphs = Glyphs::new();
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
        let glyphs = Glyphs::new();
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
        let target_names = name_path_for(&app.project.files, &target_path).expect("name path");

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
        assert_eq!(app.overlay, Some(Overlay::Help));
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
        assert_eq!(app.overlay, None);
    }

    #[test]
    fn slash_filter_uses_prompt_mode() {
        let mut app = sample_app();

        app.handle_key(key(KeyCode::Char('/'))).unwrap();
        app.handle_key(key(KeyCode::Char('c'))).unwrap();
        app.handle_key(key(KeyCode::Char('h'))).unwrap();

        assert_eq!(app.mode, Mode::Search);
        assert_eq!(app.filter.as_str(), "ch");
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
    fn q_closes_help_overlay() {
        let mut app = sample_app();
        app.overlay = Some(Overlay::Help);

        app.handle_key(key(KeyCode::Char('q'))).unwrap();

        assert_eq!(app.overlay, None);
        assert!(!app.should_quit);
    }

    #[test]
    fn q_does_not_quit_in_normal_mode() {
        // Exit is via `:q` only; the bare `q` key must not quit.
        let mut app = sample_app();
        assert_eq!(app.overlay, None);

        app.handle_key(key(KeyCode::Char('q'))).unwrap();

        assert!(!app.should_quit);
    }

    #[test]
    fn colon_q_command_quits() {
        // The command buffer holds the text after the leading `:`.
        let mut app = sample_app();
        app.mode = Mode::Command;
        app.command.set("q".to_string());

        // Submit the command line.
        let _ = app.handle_key(key(KeyCode::Enter)).unwrap();

        assert!(app.should_quit);
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
            ProjectSymbols {
                root: PathBuf::from("/workspace"),
                files: vec![
                    {
                        let mut f = SymbolNode::file(
                            "src/lib.rs",
                            vec![
                                SymbolNode::new(
                                    "parent",
                                    SymbolKind::from_lsp(2),
                                    Some(1),
                                    None,
                                    vec![
                                        SymbolNode::new(
                                            "child_a",
                                            SymbolKind::from_lsp(12),
                                            Some(2),
                                            None,
                                            Vec::new(),
                                        ),
                                        SymbolNode::new(
                                            "child_b",
                                            SymbolKind::from_lsp(12),
                                            Some(3),
                                            None,
                                            Vec::new(),
                                        ),
                                    ],
                                ),
                                SymbolNode::new(
                                    "other_parent",
                                    SymbolKind::from_lsp(23),
                                    Some(5),
                                    None,
                                    vec![SymbolNode::new(
                                        "other_child",
                                        SymbolKind::from_lsp(12),
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
