use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    time::Duration,
};

use notify::EventKind;

use crate::{error::AppResult, lsp::load_project_symbols, model::ProjectSymbols};

pub(super) const SYMBOL_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

pub(super) fn reload_worker(
    root: PathBuf,
    lsp_command: String,
    rx: Receiver<()>,
    tx: Sender<AppResult<ProjectSymbols>>,
) {
    while rx.recv().is_ok() {
        while rx.try_recv().is_ok() {}
        let result = load_project_symbols(&root, &lsp_command);
        if tx.send(result).is_err() {
            break;
        }
    }
}

pub(super) fn fs_event_should_trigger_reload(
    event: &notify::Event,
    watched: &HashSet<PathBuf>,
) -> bool {
    if !matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|p| watched.contains(p))
}
