//! Symbol-load orchestration: the reload-state types (`PendingReload`,
//! `DirtyState`), requesting a reload, and folding streamed `LoadEvent`s into
//! the project tree while preserving the user's cursor and expansion state.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::App;
use super::watcher::{SYMBOL_RELOAD_DEBOUNCE, SYMBOL_RELOAD_MAX_WAIT};
use crate::lsp::LoadEvent;
use crate::tree::{
    apply_expanded_overrides, collect_expanded_overrides, name_path_for, name_path_to_index_path,
};

/// Selection and expansion state captured when a reload begins, so the user's
/// cursor position and expand/collapse choices survive the rebuild. Held as an
/// `Option` so "a reload is in flight" is a type, not three loosely-coupled
/// fields.
pub(super) struct PendingReload {
    pub(super) expanded: HashMap<Vec<String>, bool>,
    pub(super) selection_name_path: Option<Vec<String>>,
    pub(super) selection_row: usize,
}

/// Debounce state for filesystem-driven reloads: `at` is the most recent change
/// (the 300ms settle timer) and `since` is the first un-serviced change (the
/// max-wait bound that stops sustained churn from starving reloads).
#[derive(Default)]
pub(super) struct DirtyState {
    at: Option<Instant>,
    since: Option<Instant>,
}

impl DirtyState {
    pub(super) fn mark(&mut self, now: Instant) {
        self.at = Some(now);
        self.since.get_or_insert(now);
    }

    pub(super) fn clear(&mut self) {
        *self = Self::default();
    }

    /// Whether a reload should fire: the burst has settled, or we've waited long
    /// enough that we should stop waiting for it to settle.
    pub(super) fn ready(&self) -> bool {
        self.at
            .is_some_and(|t| t.elapsed() >= SYMBOL_RELOAD_DEBOUNCE)
            || self
                .since
                .is_some_and(|t| t.elapsed() >= SYMBOL_RELOAD_MAX_WAIT)
    }

    /// Time until the next reload-readiness deadline (debounce or max-wait),
    /// for sizing the event-loop poll. `None` when not dirty.
    pub(super) fn wake_in(&self) -> Option<Duration> {
        let debounce = self
            .at
            .map(|t| SYMBOL_RELOAD_DEBOUNCE.saturating_sub(t.elapsed()));
        let max_wait = self
            .since
            .map(|t| SYMBOL_RELOAD_MAX_WAIT.saturating_sub(t.elapsed()));
        [debounce, max_wait].into_iter().flatten().min()
    }
}

impl App {
    pub(super) fn set_load_message(&mut self, prefix: &str) {
        self.message = compose_load_message(prefix, &self.project.warnings);
    }

    pub(super) fn request_reload(&mut self) {
        if self.load_in_flight {
            return;
        }
        let _ = self.load_request_tx.send(());
        self.load_in_flight = true;
    }

    pub(super) fn handle_load_event(&mut self, event: LoadEvent) {
        match event {
            LoadEvent::Started => {
                let previous_index_path = self.selected_path();
                self.pending_reload = Some(PendingReload {
                    selection_name_path: previous_index_path
                        .as_deref()
                        .and_then(|p| name_path_for(&self.project.files, p)),
                    selection_row: self.selected,
                    expanded: collect_expanded_overrides(&self.project.files),
                });

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
                if let Some(pending) = &self.pending_reload
                    && !pending.expanded.is_empty()
                {
                    apply_expanded_overrides(&mut file, &pending.expanded);
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

                if let Some(pending) = self.pending_reload.take() {
                    let restored_index_path = pending
                        .selection_name_path
                        .as_deref()
                        .and_then(|np| name_path_to_index_path(&self.project.files, np));
                    self.restore_selection(restored_index_path.as_deref(), pending.selection_row);
                }

                self.set_load_message(&format!("Loaded {} files", self.loaded_file_count));
            }
        }
    }

    pub(super) fn refresh_watched_files(&mut self) {
        self.watched_files = self
            .project
            .files
            .iter()
            .map(|node| self.root.join(&node.name))
            .collect();
    }
}

pub(super) fn compose_load_message(prefix: &str, warnings: &[String]) -> String {
    match warnings.len() {
        0 => prefix.to_string(),
        1 => format!("{prefix} — ! {}", warnings[0]),
        n => format!("{prefix} — ! {} (+{} more)", warnings[0], n - 1),
    }
}
