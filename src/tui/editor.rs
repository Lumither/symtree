//! Opening the selected symbol in an external editor. The TUI is suspended for
//! the duration so a terminal editor (`$EDITOR`) gets the screen, then re-init'd
//! when it exits.

use std::{env, mem, process::Command};

use ratatui::DefaultTerminal;

use super::App;
use crate::error::{AppContext, AppResult};
use crate::tree::SelectionTarget;

/// Suspend the TUI, open the current selection in `$EDITOR` at its line, then
/// restore the TUI. The outcome is reported via the status message.
pub(super) fn open_selection(terminal: &mut DefaultTerminal, app: &mut App) -> AppResult<()> {
    let Some(target) = app.selected_target() else {
        app.message = "Nothing selected".to_string();
        return Ok(());
    };

    ratatui::try_restore().context("failed to restore terminal before opening editor")?;
    let launch_result = launch_editor(&target);
    let restored = ratatui::try_init().context("failed to restore TUI after editor")?;
    let _ = mem::replace(terminal, restored);

    match launch_result {
        Ok(status) if status.success() => {
            app.message = format!(
                "Opened {}:{} ({})",
                target.file.display(),
                target.line,
                target.label
            );
        }
        Ok(status) => {
            app.message = format!("Editor exited with status {status}");
        }
        Err(error) => {
            app.message = format!("Failed to open editor: {error}");
        }
    }

    Ok(())
}

fn launch_editor(target: &SelectionTarget) -> AppResult<std::process::ExitStatus> {
    let editor = env::var("EDITOR").unwrap_or_else(|_| "vi".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("vi");
    let mut command = Command::new(program);
    command.args(parts);
    command.arg(format!("+{}", target.line));
    command.arg(&target.file);
    command
        .status()
        .with_context(|| format!("failed to launch editor `{editor}`"))
}
