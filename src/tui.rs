use std::{
    cell::{Ref, RefCell},
    collections::HashSet,
    env, mem,
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

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
    tree::{
        SelectionTarget, VisibleNode, flatten_visible, get_node, get_node_mut, selection_target,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlyphMode {
    Unicode,
    Ascii,
}

fn set_expanded_recursive(nodes: &mut [crate::model::SymbolNode], expanded: bool) {
    for node in nodes {
        node.expanded = expanded;
        set_expanded_recursive(&mut node.children, expanded);
    }
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
    show_warnings: bool,
    show_lsp: bool,
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
}

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
            show_warnings: false,
            show_lsp: false,
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
        };
        app.refresh_watched_files();
        if app.project.files.is_empty() {
            let _ = app.load_request_tx.send(());
            app.load_in_flight = true;
            app.message = "Indexing…".to_string();
        } else if !app.project.warnings.is_empty() {
            app.set_load_message("Loaded");
        }
        app
    }

    fn set_load_message(&mut self, prefix: &str) {
        self.message = compose_load_message(prefix, &self.project.warnings);
    }

    fn request_reload(&mut self, status: &str) {
        if self.load_in_flight {
            return;
        }
        let _ = self.load_request_tx.send(());
        self.load_in_flight = true;
        self.message = status.to_string();
    }

    fn handle_load_event(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Started => {
                let previous_path = self.selected_path();
                self.project.files.clear();
                self.project.warnings.clear();
                self.invalidate_visible();
                self.loaded_file_count = 0;
                self.discovered_file_count = 0;
                self.load_in_flight = true;
                self.preview_cache = None;
                self.preview_in_flight = None;
                self.loading_overlay_shown = false;
                if previous_path.is_some() {
                    self.selected = 0;
                }
                self.update_indexing_message();
            }
            LoadEvent::Discovered(n) => {
                self.discovered_file_count = self.discovered_file_count.saturating_add(n);
                self.update_indexing_message();
            }
            LoadEvent::FileLoaded(file) => {
                self.project.files.push(file);
                self.invalidate_visible();
                self.loaded_file_count += 1;
                self.update_indexing_message();
            }
            LoadEvent::Warning(text) => {
                self.project.warnings.push(text);
            }
            LoadEvent::Finished => {
                self.project.files.sort_by(|a, b| a.name.cmp(&b.name));
                self.invalidate_visible();
                self.load_in_flight = false;
                self.refresh_watched_files();
                self.set_load_message(&format!("Loaded {} files", self.loaded_file_count));
            }
        }
    }

    fn update_indexing_message(&mut self) {
        if !self.load_in_flight {
            return;
        }
        self.message = if self.discovered_file_count > 0 {
            format!(
                "Indexing… {}/{} files",
                self.loaded_file_count, self.discovered_file_count
            )
        } else {
            format!("Indexing… {} files", self.loaded_file_count)
        };
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
            let computed = flatten_visible(&self.project.files, &self.filter);
            *self.visible_cache.borrow_mut() = Some(computed);
        }
        Ref::map(self.visible_cache.borrow(), |opt| {
            opt.as_ref().expect("filled above")
        })
    }

    fn invalidate_visible(&self) {
        *self.visible_cache.borrow_mut() = None;
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

        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.show_help = false;
                    self.message.clear();
                }
                _ => {}
            }
            return Ok(Action::None);
        }

        if self.show_warnings {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.show_warnings = false;
                    self.message.clear();
                }
                _ => {}
            }
            return Ok(Action::None);
        }

        if self.show_lsp {
            match key.code {
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                    self.show_lsp = false;
                    self.message.clear();
                }
                _ => {}
            }
            return Ok(Action::None);
        }

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
                self.message.clear();
                Action::None
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
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
            KeyCode::Esc | KeyCode::Enter => {
                self.mode = Mode::Normal;
                Action::None
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.selected = 0;
                self.invalidate_visible();
                Action::None
            }
            KeyCode::Delete => {
                self.filter.clear();
                self.selected = 0;
                self.invalidate_visible();
                Action::None
            }
            KeyCode::Char(char) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.filter.push(char);
                    self.selected = 0;
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
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
                Action::None
            }
            KeyCode::Enter => {
                self.execute_command();
                self.mode = Mode::Normal;
                self.command.clear();
                Action::None
            }
            KeyCode::Backspace => {
                self.command.pop();
                Action::None
            }
            KeyCode::Delete => {
                self.command.clear();
                Action::None
            }
            KeyCode::Char(char) => {
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                {
                    self.command.push(char);
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
                self.message = "Help".to_string();
            }
            "warnings" | "w" => {
                if self.project.warnings.is_empty() {
                    self.message = "No warnings".to_string();
                } else {
                    self.show_warnings = true;
                    self.message.clear();
                }
            }
            "lsp" => {
                self.show_lsp = true;
                self.message.clear();
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
            app.request_reload("Syncing symbols…");
            needs_draw = true;
        }

        let mut got_load_event = false;
        while let Ok(event) = app.load_event_rx.try_recv() {
            got_load_event = true;
            app.handle_load_event(event);
        }
        if got_load_event {
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
            terminal
                .draw(|frame| render(frame, &mut app))
                .context("failed to render frame")?;
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
                    Action::Reload => app.request_reload("Reloading…"),
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
