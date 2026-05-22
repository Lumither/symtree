use std::{
    fs,
    path::{Path, PathBuf},
};

use crate::error::{AppContext, AppResult};

pub fn collect_rust_files(root: &Path) -> AppResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    visit_dir(root, &mut files)?;
    files.sort();
    Ok(files)
}

fn visit_dir(path: &Path, files: &mut Vec<PathBuf>) -> AppResult<()> {
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
            visit_dir(&path, files)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }

    Ok(())
}

fn should_skip_dir(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };

    name == ".git" || name == "target"
}
