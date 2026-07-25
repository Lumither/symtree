//! Centralized color palette. The indexed greys below were previously bare
//! integers (`Color::Indexed(238)` etc.) scattered across the render code, where
//! nobody could tell the selection grey from the popup grey from the dim grey.
//! Keeping them here gives them names and makes a future configurable theme a
//! one-file change instead of a project-wide grep.

use ratatui::style::Color;

/// Background of the selected tree row, the status bar, and the focused preview
/// line — the project's single "highlight" grey.
pub(super) const SELECTION_BG: Color = Color::Indexed(238);

/// Background of popup/autocomplete panels.
pub(super) const POPUP_BG: Color = Color::Indexed(236);

/// Background of the selected entry inside the autocomplete popup.
pub(super) const POPUP_SELECTED_BG: Color = Color::Indexed(240);

/// Foreground of unselected autocomplete entries.
pub(super) const POPUP_TEXT: Color = Color::Indexed(250);

/// Dimmed foreground used for secondary text on the status bar.
pub(super) const DIM_TEXT: Color = Color::Indexed(244);
