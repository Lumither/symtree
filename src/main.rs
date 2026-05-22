mod error;
mod lsp;
mod model;
mod project;
mod tree;
mod tui;

use std::{env, path::PathBuf, process};

use crate::error::{AppContext, AppResult, app_error};

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
    let symbols = lsp::load_project_symbols(&root, &args.lsp_command)?;
    tui::run(root, args.lsp_command, args.glyph_mode, symbols)
}

#[derive(Debug)]
struct Args {
    root: PathBuf,
    lsp_command: String,
    glyph_mode: tui::GlyphMode,
}

impl Args {
    fn parse() -> AppResult<Option<Self>> {
        let mut root = None;
        let mut lsp_command = String::from("rust-analyzer");
        let mut glyph_mode = tui::GlyphMode::Unicode;
        let mut args = env::args().skip(1);

        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-h" | "--help" => return Ok(None),
                "--ascii" => {
                    glyph_mode = tui::GlyphMode::Ascii;
                }
                "--lsp" => {
                    lsp_command = args
                        .next()
                        .ok_or_else(|| app_error("--lsp requires a command"))?;
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

        Ok(Some(Self {
            root: root.unwrap_or_else(|| PathBuf::from(".")),
            lsp_command,
            glyph_mode,
        }))
    }
}

fn print_help() {
    println!(
        "symtree - Rust symbol tree TUI\n\nUsage:\n  symtree [PROJECT_PATH] [--lsp COMMAND] [--ascii]\n\nOptions:\n  --lsp COMMAND  LSP server command, defaults to rust-analyzer\n  --ascii        Use ASCII tree markers instead of Unicode\n  -h, --help     Show CLI help\n\nInside the TUI:\n  :help          Show keymap"
    );
}
