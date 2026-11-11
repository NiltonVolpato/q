use std::path::PathBuf;

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

/// Parse CLI arguments.
///
/// # Errors
/// Returns an error if an unknown option is provided, if multiple files are provided,
/// or if no file is provided when not in prompt mode.
pub fn parse_args<I, T>(args: I) -> anyhow::Result<Option<CliArgs>>
where
    I: IntoIterator<Item = T>,
    T: Into<String>,
{
    let mut args_iter = args.into_iter().map(Into::into);
    let _program_name = args_iter.next();

    let mut mode = EditorMode::Normal;
    let mut file_arg: Option<String> = None;

    for arg in args_iter {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "qe - inline terminal editor\n\n\
                     Usage: qe [OPTIONS] [FILE]\n\n\
                     Arguments:\n  \
                       [FILE]  Path to file to edit or create (use - or omit for pipeline)\n\n\
                     Options:\n  \
                       -p, --prompt  Prompt mode (⏎ accepts, ⇧⏎ newline, ⎋ aborts)\n  \
                       -h, --help    Print help"
                );
                return Ok(None);
            }
            "-p" | "--prompt" => {
                mode = EditorMode::Prompt;
            }
            arg if arg.starts_with('-') && arg != "-" => {
                anyhow::bail!("unknown option '{arg}'. Use --help for usage information.");
            }
            _ => {
                if file_arg.is_some() {
                    anyhow::bail!(
                        "unexpected argument '{arg}'. Exactly one file argument is expected."
                    );
                }
                file_arg = Some(arg);
            }
        }
    }

    let destination = match file_arg.as_deref() {
        None | Some("-") => {
            if mode != EditorMode::Prompt {
                anyhow::bail!(
                    "no file specified. To use qe as an interactive pipeline prompt, pass -p/--prompt."
                );
            }
            OutputDestination::Stdout
        }
        Some(path_str) => OutputDestination::File(PathBuf::from(path_str)),
    };

    Ok(Some(CliArgs { mode, destination }))
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use karet_filetype::file_type_for_path;
    use karet_treesitter::language_id_from_path;

    use super::*;

    #[test]
    fn test_parse_args_normal_mode() {
        let args = vec!["qe", "src/main.rs"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Normal);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("src/main.rs"))
        );
    }

    #[test]
    fn test_parse_args_prompt_mode_with_file() {
        let args = vec!["qe", "-p", "commit.txt"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(
            parsed.destination,
            OutputDestination::File(PathBuf::from("commit.txt"))
        );
    }

    #[test]
    fn test_parse_args_prompt_mode_stdout_no_file() {
        let args = vec!["qe", "-p"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(parsed.destination, OutputDestination::Stdout);

        let args_long = vec!["qe", "--prompt"];
        let parsed_long = parse_args(args_long).unwrap().unwrap();
        assert_eq!(parsed_long.mode, EditorMode::Prompt);
        assert_eq!(parsed_long.destination, OutputDestination::Stdout);
    }

    #[test]
    fn test_parse_args_prompt_mode_stdout_explicit_dash() {
        let args = vec!["qe", "-p", "-"];
        let parsed = parse_args(args).unwrap().unwrap();
        assert_eq!(parsed.mode, EditorMode::Prompt);
        assert_eq!(parsed.destination, OutputDestination::Stdout);
    }

    #[test]
    fn test_parse_args_help() {
        let args = vec!["qe", "--help"];
        let parsed = parse_args(args).unwrap();
        assert!(parsed.is_none());
    }

    #[test]
    fn test_parse_args_missing_file_without_prompt_fails() {
        let args = vec!["qe"];
        assert!(parse_args(args).is_err());

        let args_dash = vec!["qe", "-"];
        assert!(parse_args(args_dash).is_err());
    }

    #[test]
    fn test_parse_args_unknown_flag_fails() {
        let args = vec!["qe", "--unknown", "file.txt"];
        assert!(parse_args(args).is_err());
    }

    #[test]
    fn test_parse_args_multiple_files_fails() {
        let args = vec!["qe", "file1.txt", "file2.txt"];
        assert!(parse_args(args).is_err());
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
