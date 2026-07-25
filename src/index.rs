//! Persistent symbol index: one JSON shard per source file under
//! `$XDG_CACHE_HOME/symtree/<root>-<hash>/`, keyed by the file's (mtime, size).
//! A reload streams cached shards to the UI and only queries the LSP for files
//! whose signature changed; a fully-cached repo never spawns a server.
//!
//! One shard per file, not one document per project: an edit rewrites one small
//! file and nothing needs to be held in memory to do it. The stable field names
//! also make the shards usable as input for offline analysis.

use std::{
    fs,
    hash::{DefaultHasher, Hash, Hasher},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use serde::{Deserialize, Serialize};

use crate::{
    error::{AppContext, AppResult},
    model::SymbolNode,
};

/// Bump on any shard-shape or symbol-extraction change; a mismatch is a miss.
const VERSION: u32 = 1;

/// Not a content hash: a touched-but-unchanged file is re-indexed, which costs
/// one LSP round-trip and never serves stale symbols.
pub(crate) type Signature = (u128, u64);

#[derive(Debug, Serialize, Deserialize)]
struct Shard {
    version: u32,
    /// Relative to the project root; also guards against a shard-name collision.
    path: String,
    mtime_ns: u128,
    size: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    symbols: Vec<SymbolNode>,
}

pub(crate) fn signature(path: &Path) -> Option<Signature> {
    let meta = fs::metadata(path).ok()?;
    let mtime = meta.modified().ok()?.duration_since(UNIX_EPOCH).ok()?;
    Some((mtime.as_nanos(), meta.len()))
}

/// Cached symbols for `relative_path`, if the shard was captured from a file with
/// this exact signature. Missing, corrupt, stale, or wrong version: all misses.
pub(crate) fn get(
    root: &Path,
    relative_path: &str,
    signature: Signature,
) -> Option<Vec<SymbolNode>> {
    let file = fs::File::open(shard_path(root, relative_path)).ok()?;
    let shard: Shard = serde_json::from_reader(BufReader::new(file)).ok()?;
    (shard.version == VERSION
        && shard.path == relative_path
        && (shard.mtime_ns, shard.size) == signature)
        .then_some(shard.symbols)
}

/// Temp file + rename, so an interrupted write leaves no half-parsed shard.
pub(crate) fn put(
    root: &Path,
    relative_path: &str,
    signature: Signature,
    symbols: &[SymbolNode],
) -> AppResult<()> {
    let path = shard_path(root, relative_path);
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    }
    let shard = Shard {
        version: VERSION,
        path: relative_path.to_string(),
        mtime_ns: signature.0,
        size: signature.1,
        symbols: symbols.to_vec(),
    };
    // Pid-suffixed so two symtree processes can't race on the same temp name.
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let file =
        fs::File::create(&tmp).with_context(|| format!("failed to write {}", tmp.display()))?;
    let mut writer = BufWriter::new(file);
    serde_json::to_writer(&mut writer, &shard).context("failed to serialize symbol shard")?;
    drop(writer);
    fs::rename(&tmp, &path).with_context(|| format!("failed to replace {}", path.display()))
}

/// Every shard for one project root. Deleting it forces a full re-index.
pub(crate) fn dir_for(root: &Path) -> PathBuf {
    // ponytail: DefaultHasher isn't guaranteed stable across rustc versions —
    // worst case a toolchain upgrade means one cold start.
    let mut hasher = DefaultHasher::new();
    root.hash(&mut hasher);
    let name = root.file_name().and_then(|n| n.to_str()).unwrap_or("root");
    cache_dir().join(format!("{name}-{:016x}", hasher.finish()))
}

// ponytail: shards for deleted source files are never collected — they are inert
// bytes in a cache dir. Add a sweep at load time if the leak ever matters.
fn shard_path(root: &Path, relative_path: &str) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    relative_path.hash(&mut hasher);
    dir_for(root).join(format!("{:016x}.json", hasher.finish()))
}

fn cache_dir() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("symtree")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SymbolKind;

    #[test]
    fn round_trips_and_invalidates_on_signature_change() {
        let dir = std::env::temp_dir().join(format!("symtree_idx_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        unsafe { std::env::set_var("XDG_CACHE_HOME", &dir) };
        let root = dir.join("proj");
        fs::create_dir_all(&root).unwrap();

        let file = root.join("lib.rs");
        fs::write(&file, "fn main() {}").unwrap();
        let sig = signature(&file).expect("signature");
        let symbols = vec![SymbolNode::new(
            "main",
            SymbolKind::from_lsp(12),
            Some(1),
            None,
            Vec::new(),
        )];

        put(&root, "lib.rs", sig, &symbols).expect("put");

        let hit = get(&root, "lib.rs", sig).expect("cache hit");
        assert_eq!(hit[0].name, "main");
        assert!(hit[0].expanded, "expansion defaults to open");

        // A changed signature and an unknown path both miss.
        assert!(get(&root, "lib.rs", (sig.0 + 1, sig.1)).is_none());
        assert!(get(&root, "other.rs", sig).is_none());

        let _ = fs::remove_dir_all(&dir);
    }
}
