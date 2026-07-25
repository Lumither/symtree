//! Input handling: key dispatch for every mode, the overlay key handler, and
//! command execution. All methods on `App`; only `handle_key` is reached from
//! the run loop.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{App, Mode, Overlay, autocomplete};
use crate::error::AppResult;
use crate::query;
use crate::tree::set_expanded_recursive;

impl App {
    pub(super) fn handle_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if key.kind == KeyEventKind::Release {
            return Ok(());
        }

        match self.mode {
            Mode::Normal => self.handle_normal_key(key)?,
            Mode::Search => self.handle_search_key(key),
            Mode::Command => self.handle_command_key(key),
        }
        Ok(())
    }

    /// Open `overlay`, resetting scroll and clearing the status line.
    fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_scroll = 0;
        self.message.clear();
    }

    /// If an overlay is open, handle the key (Esc/Enter/q close; j/k/d/u/g/G
    /// scroll) and report that it was consumed so the caller stops processing.
    fn handle_overlay_key(&mut self, key: KeyEvent) -> bool {
        if self.overlay.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                self.overlay = None;
                self.overlay_scroll = 0;
                self.message.clear();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                self.overlay_scroll = self.overlay_scroll.saturating_add(1);
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.overlay_scroll = self.overlay_scroll.saturating_sub(1);
            }
            KeyCode::Char('d') | KeyCode::PageDown => {
                self.overlay_scroll = self.overlay_scroll.saturating_add(10);
            }
            KeyCode::Char('u') | KeyCode::PageUp => {
                self.overlay_scroll = self.overlay_scroll.saturating_sub(10);
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.overlay_scroll = 0;
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.overlay_scroll = usize::MAX;
            }
            _ => {}
        }
        true
    }

    fn handle_normal_key(&mut self, key: KeyEvent) -> AppResult<()> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.should_quit = true;
            return Ok(());
        }

        if self.handle_overlay_key(key) {
            return Ok(());
        }

        match key.code {
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
            }
            KeyCode::Char('K') => self.move_by(-3),
            KeyCode::Char('J') => self.move_by(3),
            KeyCode::Up | KeyCode::Char('k') => self.move_up(),
            KeyCode::Down | KeyCode::Char('j') => self.move_down(),
            KeyCode::PageUp => self.page_up(),
            KeyCode::PageDown => self.page_down(),
            KeyCode::Home => self.move_home(),
            KeyCode::End => self.move_end(),
            KeyCode::Enter | KeyCode::Char(' ') => self.toggle_selected(),
            KeyCode::Left | KeyCode::Char('h') => self.move_to_parent(),
            KeyCode::Right | KeyCode::Char('l') => self.move_to_first_child(),
            KeyCode::Char('u') => self.move_to_next_sibling(),
            KeyCode::Char('i') => self.move_to_previous_sibling(),
            KeyCode::Char('/') => {
                self.mode = Mode::Search;
                self.filter.cursor_to_end();
                self.candidate_index = 0;
                self.message.clear();
            }
            KeyCode::Char(':') => {
                self.mode = Mode::Command;
                self.command.clear();
                self.candidate_index = 0;
                self.message.clear();
            }
            KeyCode::Char('r') => self.request_reload(),
            // Handled by the run loop, which has the terminal handle needed to
            // suspend the TUI while the editor runs.
            KeyCode::Char('o') => self.pending_open_selection = true,
            _ => {}
        }

        Ok(())
    }

    /// Reset selection and recompute the visible set after the filter changes.
    fn after_filter_edit(&mut self) {
        self.selected = 0;
        self.candidate_index = 0;
        self.invalidate_visible();
    }

    /// Move the autocomplete highlight by `delta`, wrapping, over the candidates
    /// produced by `candidates`.
    fn cycle_candidate(&mut self, candidates: fn(&App) -> Vec<String>, delta: isize) {
        let len = candidates(self).len();
        if len > 0 {
            let next = (self.candidate_index as isize + delta).rem_euclid(len as isize);
            self.candidate_index = next as usize;
        }
    }

    /// The autocomplete pick currently highlighted, if any.
    fn current_candidate(&self, candidates: fn(&App) -> Vec<String>) -> Option<String> {
        let cands = candidates(self);
        cands
            .get(self.candidate_index.min(cands.len().saturating_sub(1)))
            .cloned()
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.after_filter_edit();
            }
            KeyCode::Char('a') if ctrl => self.filter.cursor_to_start(),
            KeyCode::Char('e') if ctrl => self.filter.cursor_to_end(),
            KeyCode::Left if ctrl => self.filter.word_left(),
            KeyCode::Right if ctrl => self.filter.word_right(),
            KeyCode::Esc | KeyCode::Enter => {
                self.mode = Mode::Normal;
                self.candidate_index = 0;
            }
            KeyCode::Tab => {
                if let Some(pick) = self.current_candidate(autocomplete::search_candidates) {
                    let replaced = autocomplete::replace_last_token(self.filter.as_str(), &pick);
                    self.filter.set(replaced);
                    self.invalidate_visible();
                }
                self.candidate_index = 0;
            }
            KeyCode::Up => self.cycle_candidate(autocomplete::search_candidates, -1),
            KeyCode::Down => self.cycle_candidate(autocomplete::search_candidates, 1),
            KeyCode::Left => self.filter.left(),
            KeyCode::Right => self.filter.right(),
            KeyCode::Home => self.filter.cursor_to_start(),
            KeyCode::End => self.filter.cursor_to_end(),
            KeyCode::Backspace => {
                if self.filter.delete_before() {
                    self.after_filter_edit();
                }
            }
            KeyCode::Delete => {
                if self.filter.delete_at() {
                    self.after_filter_edit();
                }
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.filter.insert(ch);
                self.after_filter_edit();
            }
            _ => {}
        }
    }

    fn handle_command_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c') if ctrl => self.should_quit = true,
            KeyCode::Char('u') if ctrl => {
                self.command.clear();
                self.candidate_index = 0;
            }
            KeyCode::Char('a') if ctrl => self.command.cursor_to_start(),
            KeyCode::Char('e') if ctrl => self.command.cursor_to_end(),
            KeyCode::Left if ctrl => self.command.word_left(),
            KeyCode::Right if ctrl => self.command.word_right(),
            KeyCode::Esc => {
                self.mode = Mode::Normal;
                self.command.clear();
                self.candidate_index = 0;
            }
            KeyCode::Enter => {
                // `:query` with no argument reopens the active query for editing.
                if self.command.trim() == "query"
                    && let Some(src) = self.query_source.clone()
                {
                    self.command.set(format!("query {src}"));
                    self.candidate_index = 0;
                    return;
                }
                self.execute_command();
                self.mode = Mode::Normal;
                self.command.clear();
                self.candidate_index = 0;
            }
            KeyCode::Tab => {
                if let Some(pick) = self.current_candidate(autocomplete::command_candidates) {
                    let replaced = autocomplete::replace_last_token(self.command.as_str(), &pick);
                    self.command.set(replaced);
                }
                self.candidate_index = 0;
            }
            KeyCode::Up => self.cycle_candidate(autocomplete::command_candidates, -1),
            KeyCode::Down => self.cycle_candidate(autocomplete::command_candidates, 1),
            KeyCode::Left => self.command.left(),
            KeyCode::Right => self.command.right(),
            KeyCode::Home => self.command.cursor_to_start(),
            KeyCode::End => self.command.cursor_to_end(),
            KeyCode::Backspace => {
                if self.command.delete_before() {
                    self.candidate_index = 0;
                }
            }
            KeyCode::Delete => {
                if self.command.delete_at() {
                    self.candidate_index = 0;
                }
            }
            KeyCode::Char(ch) if !ctrl && !alt => {
                self.command.insert(ch);
                self.candidate_index = 0;
            }
            _ => {}
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
                self.open_overlay(Overlay::Help);
                self.message = "Help".to_string();
            }
            "keymap" | "keys" => {
                self.open_overlay(Overlay::Keymap);
                self.message = "Keymap".to_string();
            }
            "warnings" | "w" => {
                if self.project.warnings.is_empty() {
                    self.message = "No warnings".to_string();
                } else {
                    self.open_overlay(Overlay::Warnings);
                }
            }
            "lsp" => self.open_overlay(Overlay::Lsp),
            #[cfg(feature = "debug_perf")]
            "perf" => self.open_overlay(Overlay::Perf),
            #[cfg(feature = "debug_perf")]
            "perf reset" => {
                *self.perf.borrow_mut() = super::PerfStats::default();
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
}
