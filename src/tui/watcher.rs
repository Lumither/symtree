use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{Receiver, Sender},
    thread,
    time::Duration,
};

use notify::EventKind;

use crate::{
    languages::LanguageDef,
    lsp::{LoadEvent, lsp_is_available, stream_language},
};

pub(super) const SYMBOL_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);

pub(super) fn loader_worker(
    root: PathBuf,
    languages: Vec<LanguageDef>,
    request_rx: Receiver<()>,
    event_tx: Sender<LoadEvent>,
) {
    while request_rx.recv().is_ok() {
        while request_rx.try_recv().is_ok() {}

        if event_tx.send(LoadEvent::Started).is_err() {
            break;
        }

        let mut handles = Vec::new();
        for lang in &languages {
            // In multi-language probe mode, silently skip languages without an
            // installed binary. When the user explicitly picked one, the spawn
            // attempt itself will surface a warning via stream_language.
            if languages.len() > 1 && !lsp_is_available(&lang.lsp) {
                continue;
            }
            let root = root.clone();
            let lang = lang.clone();
            let tx = event_tx.clone();
            handles.push(thread::spawn(move || stream_language(&root, &lang, tx)));
        }
        for handle in handles {
            let _ = handle.join();
        }

        if event_tx.send(LoadEvent::Finished).is_err() {
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
