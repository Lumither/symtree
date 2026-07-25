use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{AppContext, AppResult};

pub(crate) fn collect_source_files(root: &Path, extensions: &[String]) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    // Failing to read the root itself is fatal and worth surfacing. Failures
    // deeper in the tree (a permission-denied directory, a file that vanished
    // mid-scan) are skipped instead of aborting, so one bad directory can't drop
    // every source file for a language.
    let entries =
        fs::read_dir(root).with_context(|| format!("failed to read {}", root.display()))?;
    visit_entries(entries, extensions, &mut files);
    files.sort();
    Ok(files)
}

fn visit_dir(path: &Path, extensions: &[String], files: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(path) else {
        return;
    };
    visit_entries(entries, extensions, files);
}

fn visit_entries(entries: fs::ReadDir, extensions: &[String], files: &mut Vec<PathBuf>) {
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };

        if file_type.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            visit_dir(&path, extensions, files);
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.iter().any(|target| target == ext))
        {
            files.push(path);
        }
    }
}

fn should_skip_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_skipped_dir_name)
}

/// Directory names that are never scanned for sources and whose filesystem
/// events should not trigger a reload (build output, vendored deps, VCS, etc.).
pub(crate) fn is_skipped_dir_name(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "target"
            | "node_modules"
            | ".venv"
            | "__pycache__"
            | ".idea"
            | "dist"
            | "build"
            | ".next"
            | ".cache"
    )
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn unique_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("symtree_proj_{tag}_{}", std::process::id()))
    }

    #[test]
    fn unreadable_subdir_does_not_abort_the_scan() {
        let root = unique_dir("scan");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("ok")).unwrap();
        fs::write(root.join("ok/lib.rs"), "fn main() {}").unwrap();

        // A directory we cannot read must be skipped, not fatal.
        let locked = root.join("locked");
        fs::create_dir_all(&locked).unwrap();
        fs::write(locked.join("hidden.rs"), "fn x() {}").unwrap();
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();

        let result = collect_source_files(&root, &["rs".to_string()]);

        // Restore perms so cleanup can succeed regardless of the assertion.
        let _ = fs::set_permissions(&locked, fs::Permissions::from_mode(0o755));
        let files = result.expect("scan should succeed despite the unreadable subdir");
        let _ = fs::remove_dir_all(&root);

        assert!(
            files.iter().any(|p| p.ends_with("ok/lib.rs")),
            "readable sibling file should still be collected: {files:?}"
        );
        assert!(
            !files.iter().any(|p| p.ends_with("locked/hidden.rs")),
            "file under the unreadable dir is not reachable"
        );
    }

    #[test]
    fn unreadable_root_is_fatal() {
        let root = unique_dir("root");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        let result = collect_source_files(&root, &["rs".to_string()]);

        let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o755));
        let _ = fs::remove_dir_all(&root);
        assert!(
            result.is_err(),
            "an unreadable root should surface an error"
        );
    }
}
