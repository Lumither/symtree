use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{AppContext, AppResult};

pub fn collect_source_files(root: &Path, extensions: &[String]) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    visit_dir(root, extensions, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit_dir(path: &Path, extensions: &[String], files: &mut Vec<PathBuf>) -> AppResult<()> {
    for entry in fs::read_dir(path).with_context(|| format!("failed to read {}", path.display()))? {
        let entry = entry.with_context(|| format!("failed to read entry in {}", path.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .with_context(|| format!("failed to inspect {}", path.display()))?;

        if file_type.is_dir() {
            if should_skip_dir(&path) {
                continue;
            }
            visit_dir(&path, extensions, files)?;
        } else if file_type.is_file()
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| extensions.iter().any(|target| target == ext))
        {
            files.push(path);
        }
    }

    Ok(())
}

fn should_skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
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
