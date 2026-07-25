mod error;
mod index;
mod languages;
mod lsp;
mod model;
mod project;
mod query;
mod tree;
mod tui;

use std::{env, path::PathBuf, process};

use crate::error::{AppContext, AppResult, app_error};
use crate::languages::LanguageDef;

fn main() {
    if let Err(error) = run() {
        eprintln!("symtree: {error}");
        process::exit(1);
    }
}

fn run() -> AppResult<()> {
    let Some(args) = Args::parse()? else {
        print_help();
        return Ok(());
    };

    let root = args
        .root
        .canonicalize()
        .with_context(|| format!("failed to resolve project root {}", args.root.display()))?;
    if args.reindex {
        let _ = std::fs::remove_dir_all(index::dir_for(&root));
    }
    let empty = model::ProjectSymbols {
        root: root.clone(),
        files: Vec::new(),
        warnings: Vec::new(),
    };
    tui::run(root, args.languages, empty)
}

#[derive(Debug)]
struct Args {
    root: PathBuf,
    languages: Vec<LanguageDef>,
    reindex: bool,
}

impl Args {
    fn parse() -> AppResult<Option<Self>> {
        Self::parse_from(env::args().skip(1))
    }

    /// Parse from an explicit argument iterator (the program name already
    /// stripped). Separated from `parse` so the flag handling can be tested
    /// without faking `env::args()`.
    fn parse_from<I: Iterator<Item = String>>(args: I) -> AppResult<Option<Self>> {
        let mut root: Option<PathBuf> = None;
        let mut lang_names: Option<Vec<String>> = None;
        let mut override_lsp: Option<String> = None;
        let mut override_ext: Option<Vec<String>> = None;
        let mut reindex = false;
        let mut args = args;

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--reindex" => reindex = true,
                "--lang" => {
                    let value = args
                        .next()
                        .ok_or_else(|| app_error("--lang requires a comma-separated list"))?;
                    lang_names = Some(value.split(',').map(|s| s.trim().to_string()).collect());
                }
                "--lsp" => {
                    override_lsp = Some(
                        args.next()
                            .ok_or_else(|| app_error("--lsp requires a command"))?,
                    );
                }
                "--ext" => {
                    let value = args
                        .next()
                        .ok_or_else(|| app_error("--ext requires a comma-separated list"))?;
                    override_ext = Some(value.split(',').map(|s| s.trim().to_string()).collect());
                }
                _ if arg.starts_with('-') => {
                    return Err(app_error(format!("unknown option `{arg}`")));
                }
                _ => {
                    if root.is_some() {
                        return Err(app_error("only one project path can be provided"));
                    }
                    root = Some(PathBuf::from(arg));
                }
            }
        }

        let languages = resolve_languages(lang_names, override_lsp, override_ext)?;

        Ok(Some(Self {
            root: root.unwrap_or_else(|| PathBuf::from(".")),
            languages,
            reindex,
        }))
    }
}

fn resolve_languages(
    names: Option<Vec<String>>,
    override_lsp: Option<String>,
    override_ext: Option<Vec<String>>,
) -> AppResult<Vec<LanguageDef>> {
    // Explicit single-LSP override: must specify both --lsp and --ext.
    if let (Some(lsp), Some(extensions)) = (override_lsp.as_ref(), override_ext.as_ref()) {
        return Ok(vec![LanguageDef {
            lsp: lsp.clone(),
            extensions: extensions.clone(),
            language_id: None,
        }]);
    }

    if let Some(names) = names {
        if names.is_empty() {
            return Err(app_error("--lang list is empty"));
        }
        let mut langs = Vec::new();
        for name in &names {
            let def = languages::lookup(name)
                .ok_or_else(|| app_error(format!("unknown language `{name}`")))?;
            langs.push(def);
        }
        // --lsp without --ext, combined with --lang: override LSP command on the single language.
        if let Some(lsp) = override_lsp
            && langs.len() == 1
        {
            langs[0].lsp = lsp;
        }
        return Ok(langs);
    }

    if let Some(lsp) = override_lsp {
        // --lsp alone, no --ext, no --lang: try to find a registry entry by lsp program name.
        let program = languages::lsp_program(&lsp);
        if let Some(def) = languages::all()
            .into_iter()
            .find(|d| languages::lsp_program(&d.lsp) == program || d.lsp == lsp)
        {
            return Ok(vec![LanguageDef {
                lsp,
                extensions: def.extensions,
                language_id: def.language_id,
            }]);
        }
        return Err(app_error(
            "could not infer extensions from --lsp; pass --ext too",
        ));
    }

    // Default: every registered language.
    Ok(languages::all())
}

fn print_help() {
    println!(
        "symtree - multi-language symbol tree TUI\n\nUsage:\n  symtree [PROJECT_PATH] [--lang LIST] [--lsp COMMAND] [--ext LIST]\n\nOptions:\n  --lang LIST    comma-separated language ids (e.g. rust,python)\n  --lsp COMMAND  override LSP command for the single language selected\n  --ext LIST     comma-separated file extensions (with --lsp for a custom language)\n  --reindex      discard the cached symbol index for this project first\n  -h, --help     Show CLI help\n\nWith no flags, symtree probes every registered language found under PROJECT_PATH.\n\nSymbols are cached per file under $XDG_CACHE_HOME/symtree/ and reused while the\nfile's mtime and size are unchanged, so only edited files are re-queried.\n\nEnvironment:\n  LSP_SEARCH_PATH  prepended to PATH when locating/spawning LSP binaries\n                   (e.g. ~/.local/share/nvim/mason/bin)\n\nInside the TUI:\n  :help          show keymap\n  :q             quit"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> AppResult<Option<Args>> {
        Args::parse_from(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_defaults_to_cwd_and_all_languages() {
        let parsed = parse(&[]).unwrap().expect("not help");
        assert_eq!(parsed.root, PathBuf::from("."));
        assert_eq!(parsed.languages.len(), languages::all().len());
    }

    #[test]
    fn positional_path_is_used_as_root() {
        let parsed = parse(&["/some/proj"]).unwrap().expect("not help");
        assert_eq!(parsed.root, PathBuf::from("/some/proj"));
    }

    #[test]
    fn help_flag_returns_none() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["-h"]).unwrap().is_none());
    }

    #[test]
    fn unknown_flag_and_duplicate_path_error() {
        assert!(parse(&["--nope"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["--lang"]).is_err()); // missing value
    }

    #[test]
    fn lang_selects_named_languages() {
        let parsed = parse(&["--lang", "rust"]).unwrap().expect("not help");
        assert_eq!(parsed.languages.len(), 1);
        assert_eq!(parsed.languages[0].lsp, "rust-analyzer");
    }

    // resolve_languages precedence table.

    #[test]
    fn lsp_and_ext_make_a_custom_language() {
        let langs =
            resolve_languages(None, Some("my-ls".into()), Some(vec!["foo".into()])).unwrap();
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].lsp, "my-ls");
        assert_eq!(langs[0].extensions, vec!["foo".to_string()]);
    }

    #[test]
    fn lang_plus_lsp_overrides_the_single_language_command() {
        let langs =
            resolve_languages(Some(vec!["rust".into()]), Some("my-ra".into()), None).unwrap();
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].lsp, "my-ra");
        assert_eq!(langs[0].extensions, vec!["rs".to_string()]);
    }

    #[test]
    fn lsp_alone_infers_extensions_from_registry() {
        let langs = resolve_languages(None, Some("rust-analyzer".into()), None).unwrap();
        assert_eq!(langs.len(), 1);
        assert_eq!(langs[0].extensions, vec!["rs".to_string()]);
    }

    #[test]
    fn lsp_alone_unknown_program_errors() {
        assert!(resolve_languages(None, Some("totally-unknown-ls".into()), None).is_err());
    }

    #[test]
    fn empty_and_unknown_lang_lists_error() {
        assert!(resolve_languages(Some(vec![]), None, None).is_err());
        assert!(resolve_languages(Some(vec!["klingon".into()]), None, None).is_err());
    }

    #[test]
    fn no_overrides_returns_every_language() {
        let langs = resolve_languages(None, None, None).unwrap();
        assert_eq!(langs.len(), languages::all().len());
    }
}
