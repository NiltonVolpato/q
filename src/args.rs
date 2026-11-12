use std::path::{Path, PathBuf};

use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::keymap::EditorMode;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OutputDestination {
    File(PathBuf),
    Stdout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliArgs {
    pub mode: EditorMode,
    pub destination: OutputDestination,
}

#[derive(Parser, Debug)]
#[command(name = "q", about = "Inline terminal editor", arg_required_else_help = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Edit a file in place
    Edit(EditArgs),
    /// Prompt mode (⏎ accepts, ⇧⏎ newline, ⎋ aborts)
    Prompt(PromptArgs),
}

#[derive(Parser, Debug)]
#[command(about = "Edit a file in place")]
struct EditArgs {
    /// Path to file to edit or create
    file: PathBuf,
}

#[derive(Parser, Debug)]
#[command(about = "Prompt mode (⏎ accepts, ⇧⏎ newline, ⎋ aborts)")]
struct PromptArgs {
    /// Path to file to write, or omit / '-' for stdout
    file: Option<String>,
}

impl EditArgs {
    fn into_cli_args(self) -> anyhow::Result<CliArgs> {
        if self.file.as_os_str() == "-" {
            anyhow::bail!("'-' is only supported in prompt mode.");
        }
        Ok(CliArgs {
            mode: EditorMode::Normal,
            destination: OutputDestination::File(self.file),
        })
    }
}

impl PromptArgs {
    fn into_cli_args(self) -> CliArgs {
        let destination = match self.file.as_deref() {
            None | Some("-") => OutputDestination::Stdout,
            Some(path) => OutputDestination::File(PathBuf::from(path)),
        };
        CliArgs {
            mode: EditorMode::Prompt,
            destination,
        }
    }
}

fn parse_alias<P: CommandFactory + FromArgMatches>(
    bin_name: &str,
    rest: impl Iterator<Item = String>,
) -> Result<P, clap::Error> {
    let args = std::iter::once(bin_name.to_string()).chain(rest);
    let matches = P::command().bin_name(bin_name).try_get_matches_from(args)?;
    P::from_arg_matches(&matches)
}

fn handle_clap_result<T>(result: Result<T, clap::Error>) -> anyhow::Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(err)
            if matches!(
                err.kind(),
                ErrorKind::DisplayHelp
                    | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
                    | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = err.print();
            Ok(None)
        }
        Err(err) => Err(err.into()),
    }
}

/// Parse CLI arguments, dispatching on `argv[0]` aliases (`qe`, `qp`) or `q` subcommands.
///
/// Returns `Ok(None)` when help/version was displayed and the process should exit cleanly.
///
/// # Errors
/// Returns an error if arguments are invalid or a required file is missing.
pub fn parse_args<I, T>(args: I) -> anyhow::Result<Option<CliArgs>>
where
    I: IntoIterator<Item = T>,
    T: Into<String>,
{
    let mut args_iter = args.into_iter().map(Into::into);
    let program = args_iter.next().unwrap_or_else(|| "q".to_string());
    let stem = Path::new(&program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("q");

    match stem {
        "qp" => {
            let Some(prompt_args) = handle_clap_result(parse_alias::<PromptArgs>("qp", args_iter))?
            else {
                return Ok(None);
            };
            Ok(Some(prompt_args.into_cli_args()))
        }
        "qe" => {
            let Some(edit_args) = handle_clap_result(parse_alias::<EditArgs>("qe", args_iter))?
            else {
                return Ok(None);
            };
            Ok(Some(edit_args.into_cli_args()?))
        }
        _ => {
            let full_args = std::iter::once(program).chain(args_iter);
            let Some(cli) = handle_clap_result(Cli::try_parse_from(full_args))? else {
                return Ok(None);
            };
            match cli.command {
                Command::Edit(edit_args) => Ok(Some(edit_args.into_cli_args()?)),
                Command::Prompt(prompt_args) => Ok(Some(prompt_args.into_cli_args())),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use karet_filetype::file_type_for_path;
    use karet_treesitter::language_id_from_path;

    use super::*;

    #[test]
    fn test_q_edit_subcommand() {
        let parsed = parse_args(["q", "edit", "src/main.rs"]).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Normal);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn test_qe_alias() {
        let parsed = parse_args(["/usr/local/bin/qe", "src/main.rs"])
            .unwrap()
            .unwrap();
        assert_eq!(parsed.mode, EditorMode::Normal);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn test_q_prompt_subcommand() {
        let parsed_stdout = parse_args(["q", "prompt"]).unwrap().unwrap();
        assert_eq!(parsed_stdout.mode, EditorMode::Prompt);
        assert_eq!(parsed_stdout.destination, OutputDestination::Stdout);

        let parsed_dash = parse_args(["q", "prompt", "-"]).unwrap().unwrap();
        assert_eq!(parsed_dash.mode, EditorMode::Prompt);
        assert_eq!(parsed_dash.destination, OutputDestination::Stdout);

        let parsed_file = parse_args(["q", "prompt", "commit.txt"]).unwrap().unwrap();
        assert_eq!(parsed_file.mode, EditorMode::Prompt);
        assert_eq!(
            parsed_file.destination,
            OutputDestination::File(PathBuf::from("commit.txt"))
        );
    }

    #[test]
    fn test_qp_alias() {
        let parsed_stdout = parse_args(["qp"]).unwrap().unwrap();
        assert_eq!(parsed_stdout.mode, EditorMode::Prompt);
        assert_eq!(parsed_stdout.destination, OutputDestination::Stdout);

        let parsed_dash = parse_args(["/usr/local/bin/qp", "-"]).unwrap().unwrap();
        assert_eq!(parsed_dash.mode, EditorMode::Prompt);
        assert_eq!(parsed_dash.destination, OutputDestination::Stdout);

        let parsed_file = parse_args(["qp", "commit.txt"]).unwrap().unwrap();
        assert_eq!(parsed_file.mode, EditorMode::Prompt);
        assert_eq!(
            parsed_file.destination,
            OutputDestination::File(PathBuf::from("commit.txt"))
        );
    }

    #[test]
    fn test_help_and_no_args() {
        assert!(parse_args(["q"]).unwrap().is_none());
        assert!(parse_args(["q", "--help"]).unwrap().is_none());
        assert!(parse_args(["q", "help"]).unwrap().is_none());
        assert!(parse_args(["q", "help", "edit"]).unwrap().is_none());
        assert!(parse_args(["q", "edit", "--help"]).unwrap().is_none());
        assert!(parse_args(["q", "prompt", "--help"]).unwrap().is_none());
        assert!(parse_args(["qe", "--help"]).unwrap().is_none());
        assert!(parse_args(["qp", "--help"]).unwrap().is_none());
    }

    #[test]
    fn test_q_requires_subcommand() {
        assert!(parse_args(["q", "src/main.rs"]).is_err());
        assert!(parse_args(["q", "unknown"]).is_err());
    }

    #[test]
    fn test_edit_missing_or_invalid_file_fails() {
        assert!(parse_args(["q", "edit"]).is_err());
        assert!(parse_args(["q", "edit", "-"]).is_err());
        assert!(parse_args(["qe"]).is_err());
        assert!(parse_args(["qe", "-"]).is_err());
    }

    #[test]
    fn test_unknown_flag_and_extra_args_fail() {
        assert!(parse_args(["q", "edit", "--unknown", "file.txt"]).is_err());
        assert!(parse_args(["q", "edit", "file1.txt", "file2.txt"]).is_err());
        assert!(parse_args(["qe", "-p", "file.txt"]).is_err());
        assert!(parse_args(["qp", "--unknown"]).is_err());
    }

    #[test]
    fn test_language_detection_for_multiple_languages() {
        assert_eq!(file_type_for_path(Path::new("test.rs")).name(), "Rust");
        assert!(language_id_from_path(Path::new("test.rs")).is_some());

        assert_eq!(file_type_for_path(Path::new("script.py")).name(), "Python");
        assert!(language_id_from_path(Path::new("script.py")).is_some());

        assert_eq!(file_type_for_path(Path::new("app.js")).name(), "JavaScript");
        assert!(language_id_from_path(Path::new("app.js")).is_some());

        assert_eq!(file_type_for_path(Path::new("main.go")).name(), "Go");
        assert!(language_id_from_path(Path::new("main.go")).is_some());

        assert_eq!(
            file_type_for_path(Path::new("README.md")).name(),
            "Markdown"
        );
        assert!(language_id_from_path(Path::new("README.md")).is_some());
    }
}
