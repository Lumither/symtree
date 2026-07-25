use std::{
    collections::HashSet,
    path::{Component, Path, PathBuf},
    sync::mpsc::{Receiver, Sender},
    thread,
    time::Duration,
};

use notify::EventKind;

use crate::{
    languages::LanguageDef,
    lsp::{LoadEvent, lsp_is_available, stream_language},
    project::is_skipped_dir_name,
};

pub(super) const SYMBOL_RELOAD_DEBOUNCE: Duration = Duration::from_millis(300);
/// Upper bound on how long sustained file churn may postpone a reload. Without
/// this, resetting the debounce on every event could starve reloads forever.
pub(super) const SYMBOL_RELOAD_MAX_WAIT: Duration = Duration::from_secs(3);

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
    root: &Path,
    extensions: &HashSet<String>,
    watched: &HashSet<PathBuf>,
) -> bool {
    if !matches!(
        event.kind,
        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
    ) {
        return false;
    }
    event.paths.iter().any(|path| {
        // Already-loaded files always count (covers modify/remove regardless of
        // how the extension set is configured).
        watched.contains(path)
            // Otherwise an untracked path counts only if it is a source file we
            // would have loaded: right extension, under root, not in a skipped
            // directory. This is what lets brand-new files trigger a reload.
            || is_relevant_source(path, root, extensions)
    })
}

fn is_relevant_source(path: &Path, root: &Path, extensions: &HashSet<String>) -> bool {
    let has_source_ext = path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| extensions.contains(ext));
    if !has_source_ext {
        return false;
    }
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    !relative.components().any(|component| match component {
        Component::Normal(name) => name.to_str().is_some_and(is_skipped_dir_name),
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind, RemoveKind};

    fn exts() -> HashSet<String> {
        ["rs".to_string()].into_iter().collect()
    }

    fn event(kind: EventKind, path: &Path) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![path.to_path_buf()],
            attrs: Default::default(),
        }
    }

    #[test]
    fn new_untracked_source_file_triggers_reload() {
        let root = Path::new("/proj");
        let watched = HashSet::new(); // nothing loaded yet knows about this file
        let new_file = root.join("src/brand_new.rs");
        assert!(fs_event_should_trigger_reload(
            &event(EventKind::Create(CreateKind::File), &new_file),
            root,
            &exts(),
            &watched,
        ));
    }

    #[test]
    fn modified_tracked_file_triggers_reload() {
        let root = Path::new("/proj");
        let tracked = root.join("src/lib.rs");
        let watched: HashSet<PathBuf> = [tracked.clone()].into_iter().collect();
        assert!(fs_event_should_trigger_reload(
            &event(EventKind::Modify(ModifyKind::Any), &tracked),
            root,
            &exts(),
            &watched,
        ));
    }

    #[test]
    fn removed_source_file_triggers_reload() {
        let root = Path::new("/proj");
        let watched = HashSet::new();
        let removed = root.join("src/gone.rs");
        assert!(fs_event_should_trigger_reload(
            &event(EventKind::Remove(RemoveKind::File), &removed),
            root,
            &exts(),
            &watched,
        ));
    }

    #[test]
    fn events_in_skipped_dirs_are_ignored() {
        let root = Path::new("/proj");
        let watched = HashSet::new();
        for dir in ["target", "node_modules", ".git"] {
            let path = root.join(dir).join("generated.rs");
            assert!(
                !fs_event_should_trigger_reload(
                    &event(EventKind::Create(CreateKind::File), &path),
                    root,
                    &exts(),
                    &watched,
                ),
                "{dir} events should not trigger a reload"
            );
        }
    }

    #[test]
    fn non_source_extensions_are_ignored() {
        let root = Path::new("/proj");
        let watched = HashSet::new();
        let log = root.join("build.log");
        assert!(!fs_event_should_trigger_reload(
            &event(EventKind::Create(CreateKind::File), &log),
            root,
            &exts(),
            &watched,
        ));
    }

    #[test]
    fn non_mutating_event_kinds_are_ignored() {
        let root = Path::new("/proj");
        let watched = HashSet::new();
        let file = root.join("src/lib.rs");
        assert!(!fs_event_should_trigger_reload(
            &event(EventKind::Access(notify::event::AccessKind::Read), &file),
            root,
            &exts(),
            &watched,
        ));
    }
}
