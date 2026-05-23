use std::{collections::BTreeMap, sync::LazyLock};

use serde::Deserialize;

const REGISTRY_JSON: &str = include_str!("../assets/languages.json");

#[derive(Debug, Clone, Deserialize)]
pub struct LanguageDef {
    pub lsp: String,
    pub extensions: Vec<String>,
    #[serde(default)]
    pub language_id: Option<String>,
}

pub static REGISTRY: LazyLock<BTreeMap<String, LanguageDef>> = LazyLock::new(|| {
    serde_json::from_str(REGISTRY_JSON).expect("invalid embedded assets/languages.json")
});

pub fn lookup(name: &str) -> Option<LanguageDef> {
    REGISTRY.get(name).cloned()
}

pub fn all() -> Vec<LanguageDef> {
    REGISTRY.values().cloned().collect()
}

pub fn lsp_program(command: &str) -> &str {
    command.split_whitespace().next().unwrap_or(command)
}

pub fn lang_for_extension(ext: &str) -> Option<&'static str> {
    REGISTRY
        .iter()
        .find(|(_, def)| def.extensions.iter().any(|x| x == ext))
        .map(|(name, _)| name.as_str())
}

pub fn lang_for_path(path: &str) -> Option<&'static str> {
    std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .and_then(lang_for_extension)
}
