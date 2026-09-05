//! `whence completions` — printing a shell completion script, or installing it
//! where the shell already looks.

use super::Cli;
use anyhow::{Context, Result};
use clap::{CommandFactory, ValueEnum};
use std::path::PathBuf;

/// The shells whence ships completions for. Deliberately not
/// `clap_complete::Shell`: this is the set that has an install location worth
/// knowing, and offering a shell we cannot install for is worse than not
/// listing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Shell {
    Bash,
    Zsh,
    Fish,
}

impl Shell {
    fn generator(self) -> clap_complete::Shell {
        match self {
            Shell::Bash => clap_complete::Shell::Bash,
            Shell::Zsh => clap_complete::Shell::Zsh,
            Shell::Fish => clap_complete::Shell::Fish,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Zsh => "zsh",
            Shell::Fish => "fish",
        }
    }

    /// From `$SHELL`, which is the login shell rather than necessarily the one
    /// running this command — close enough to be a useful default, and the
    /// positional argument overrides it.
    fn detect() -> Option<Self> {
        let shell = std::env::var_os("SHELL")?;
        let file = std::path::Path::new(&shell)
            .file_name()?
            .to_str()?
            .to_owned();
        Shell::value_variants()
            .iter()
            .copied()
            .find(|s| s.name() == file)
    }

    /// Where this shell loads user completions from. bash and fish pick the
    /// file up on their own; zsh only reads directories that are on `$fpath`.
    fn install_path(self, command: &str) -> Result<PathBuf> {
        Ok(match self {
            Shell::Bash => xdg_dir("XDG_DATA_HOME", ".local/share")?
                .join("bash-completion/completions")
                .join(command),
            Shell::Zsh => xdg_dir("XDG_DATA_HOME", ".local/share")?
                .join("zsh/site-functions")
                .join(format!("_{command}")),
            Shell::Fish => xdg_dir("XDG_CONFIG_HOME", ".config")?
                .join("fish/completions")
                .join(format!("{command}.fish")),
        })
    }
}

/// `$VAR` if it names a path, else `$HOME/<fallback>`.
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf> {
    match whence::home::non_empty(var) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => Ok(whence::home::dir()
            .context("neither $HOME nor $XDG_* is set")?
            .join(fallback)),
    }
}

pub fn run(shell: Option<Shell>, install: bool) -> Result<()> {
    let shell = match shell {
        Some(shell) => shell,
        None => Shell::detect().context(
            "could not tell which shell you use from $SHELL; name one: whence completions fish",
        )?,
    };
    if !install {
        write_script(shell, &mut std::io::stdout());
        return Ok(());
    }

    let command = Cli::command().get_name().to_string();
    let path = shell.install_path(&command)?;
    let dir = path.parent().expect("the install path always has a parent");
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let mut script = Vec::new();
    write_script(shell, &mut script);
    std::fs::write(&path, &script)
        .with_context(|| format!("could not write {}", path.display()))?;
    println!(
        "installed {} completions to {}",
        shell.name(),
        path.display()
    );
    match shell {
        // bash-completion and fish both scan their directory on startup.
        Shell::Bash | Shell::Fish => println!("open a new shell to pick them up"),
        // zsh reads only what is on $fpath, and a fresh directory is not.
        Shell::Zsh => println!(
            "add this to ~/.zshrc above `compinit`, if it is not there already:\n  \
             fpath=({} $fpath)",
            dir.display()
        ),
    }
    Ok(())
}

/// The script itself. Generated from the same `Cli` derive the parser uses, so
/// a new flag is completable the moment it exists — there is no second list of
/// options to keep in step.
fn write_script(shell: Shell, out: &mut impl std::io::Write) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell.generator(), &mut command, &name, out);
    // clap's fish generator emits candidates for options but not for a
    // positional's own values, so `whence completions <TAB>` would fall back to
    // file names. The other shells get this from the generator itself.
    if shell == Shell::Fish {
        let shells: Vec<&str> = Shell::value_variants().iter().map(|s| s.name()).collect();
        let _ = writeln!(
            out,
            "complete -c {name} -n \"__fish_{name}_using_subcommand completions\" -f -a \"{}\"",
            shells.join(" ")
        );
    }
}
