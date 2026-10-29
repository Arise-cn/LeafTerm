//! Session-local Shell setup; user startup files are never rewritten.

use std::{ffi::OsStr, path::Path};

use anyhow::{Context, Result};
use portable_pty::CommandBuilder;
use tempfile::TempDir;

const STARTUP: &str = include_str!("shell/startup.zsh");
const COMPLETION: &str = include_str!("shell/leafterm.zsh");

pub fn configure(command: &mut CommandBuilder, shell: &OsStr) -> Result<Option<TempDir>> {
    if Path::new(shell).file_name() != Some(OsStr::new("zsh"))
        || command.get_env("LEAFTERM_SHELL_INTEGRATION") == Some(OsStr::new("0"))
    {
        return Ok(None);
    }

    let directory = tempfile::Builder::new()
        .prefix("leafterm-zsh-")
        .tempdir()
        .context("Create session-local zsh setup")?;
    std::fs::write(directory.path().join(".zshenv"), STARTUP)?;
    std::fs::write(directory.path().join("leafterm.zsh"), COMPLETION)?;

    if let Some(original) = command.get_env("ZDOTDIR").map(ToOwned::to_owned) {
        command.env("LEAFTERM_ORIGINAL_ZDOTDIR", original);
        command.env("LEAFTERM_ZDOTDIR_WAS_SET", "1");
    } else {
        command.env_remove("LEAFTERM_ORIGINAL_ZDOTDIR");
        command.env("LEAFTERM_ZDOTDIR_WAS_SET", "0");
    }
    command.env("LEAFTERM_SHELL_DIR", directory.path());
    command.env("ZDOTDIR", directory.path());
    Ok(Some(directory))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zsh_startup_keeps_user_files_and_zdotdir_redirection() {
        let fixture = tempfile::tempdir().unwrap();
        let original = fixture.path().join("original");
        let redirected = fixture.path().join("custom config");
        std::fs::create_dir_all(&original).unwrap();
        std::fs::create_dir_all(&redirected).unwrap();
        let events = fixture.path().join("events");
        std::fs::write(original.join(".zshenv"), "print -r -- env >> \"$LEAFTERM_TEST_EVENTS\"\nexport ZDOTDIR=$LEAFTERM_TEST_REDIRECT\n").unwrap();
        for name in ["zprofile", "zshrc", "zlogin", "zlogout"] {
            std::fs::write(
                redirected.join(format!(".{name}")),
                format!("print -r -- {name} >> \"$LEAFTERM_TEST_EVENTS\"\nHISTFILE=/dev/null\n"),
            )
            .unwrap();
        }
        let mut command = CommandBuilder::new("/bin/zsh");
        command.env("ZDOTDIR", &original);
        let integration = configure(&mut command, OsStr::new("/bin/zsh"))
            .unwrap()
            .unwrap();
        // Exercise the startup shim independently of completion widgets.
        std::fs::write(
            integration.path().join("leafterm.zsh"),
            "print -r -- completion >> \"$LEAFTERM_TEST_EVENTS\"\n",
        )
        .unwrap();
        let output = std::process::Command::new("/bin/zsh")
            .args(["-lic", "_leafterm_deferred_completion_init; print -r -- \"$ZDOTDIR\"; print -r -- ${+LEAFTERM_SHELL_DIR}${+LEAFTERM_ORIGINAL_ZDOTDIR}${+LEAFTERM_ZDOTDIR_WAS_SET}; print -r -- ${precmd_functions[(Ie)_leafterm_deferred_completion_init]}"])
            .envs(command.iter_extra_env_as_str())
            .env("TERM_PROGRAM", "LeafTerm")
            .env("LEAFTERM_TEST_EVENTS", &events)
            .env("LEAFTERM_TEST_REDIRECT", &redirected)
            .output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stdout),
            format!("{}\n000\n0\n", redirected.display())
        );
        assert_eq!(
            std::fs::read_to_string(events).unwrap(),
            "env\nzprofile\nzshrc\nzlogin\ncompletion\nzlogout\n"
        );
    }

    #[test]
    fn other_shells_and_explicit_opt_out_keep_original_startup() {
        for shell in ["/bin/bash", "/bin/zsh"] {
            let mut command = CommandBuilder::new(shell);
            command.env("ZDOTDIR", "/original/config");
            if shell.ends_with("zsh") {
                command.env("LEAFTERM_SHELL_INTEGRATION", "0");
            }
            assert!(
                configure(&mut command, OsStr::new(shell))
                    .unwrap()
                    .is_none()
            );
            assert_eq!(
                command.get_env("ZDOTDIR"),
                Some(OsStr::new("/original/config"))
            );
        }
    }
}
