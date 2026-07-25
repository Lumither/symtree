use std::{collections::BTreeMap, sync::LazyLock};

use serde::Deserialize;

const REGISTRY_JSON: &str = include_str!("../assets/languages.json");

#[derive(Debug, Clone, Deserialize)]
pub(crate) struct LanguageDef {
    pub lsp: String,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub language_id: Option<String>,
}

static REGISTRY: LazyLock<BTreeMap<String, LanguageDef>> = LazyLock::new(|| {
    serde_json::from_str(REGISTRY_JSON).expect("invalid embedded assets/languages.json")
});

pub(crate) fn lookup(name: &str) -> Option<LanguageDef> {
    REGISTRY.get(name).cloned()
}

/// All registered language names, in sorted order — for completion and listing.
pub(crate) fn names() -> impl Iterator<Item = &'static str> {
    REGISTRY.keys().map(String::as_str)
}

pub(crate) fn all() -> Vec<LanguageDef> {
    REGISTRY.values().cloned().collect()
}

pub(crate) fn lsp_program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

pub(crate) fn lang_for_extension(ext: &str) -> Option<&'static str> {
    REGISTRY
        .iter()
        .find(|(_, def)| def.extensions.iter().any(|x| x == ext))
        .map(|(name, _)| name.as_str())
}

pub(crate) fn lang_for_path(path: &str) -> Option<&'static str> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(lang_for_extension)
}
