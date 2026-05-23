mod error;
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
    let empty = model::ProjectSymbols {
        root: root.clone(),
        files: Vec::new(),
        warnings: Vec::new(),
    };
    tui::run(root, args.languages, args.glyph_mode, empty)
}

#[derive(Debug)]
struct Args {
    root: PathBuf,
    languages: Vec<LanguageDef>,
    glyph_mode: tui::GlyphMode,
}

impl Args {
    fn parse() -> AppResult<Option<Self>> {
        let mut root: Option<PathBuf> = None;
        let mut lang_names: Option<Vec<String>> = None;
        let mut override_lsp: Option<String> = None;
        let mut override_ext: Option<Vec<String>> = None;
        let mut glyph_mode = tui::GlyphMode::Unicode;
        let mut args = env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--ascii" => glyph_mode = tui::GlyphMode::Ascii,
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
            glyph_mode,
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
        "symtree - multi-language symbol tree TUI\n\nUsage:\n  symtree [PROJECT_PATH] [--lang LIST] [--lsp COMMAND] [--ext LIST] [--ascii]\n\nOptions:\n  --lang LIST    comma-separated language ids (e.g. rust,python)\n  --lsp COMMAND  override LSP command for the single language selected\n  --ext LIST     comma-separated file extensions (with --lsp for a custom language)\n  --ascii        Use ASCII tree markers instead of Unicode\n  -h, --help     Show CLI help\n\nWith no flags, symtree probes every registered language found under PROJECT_PATH.\n\nEnvironment:\n  LSP_SEARCH_PATH  prepended to PATH when locating/spawning LSP binaries\n                   (e.g. ~/.local/share/nvim/mason/bin)\n\nInside the TUI:\n  :help          show keymap\n  :q             quit"
    );
}
