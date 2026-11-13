mod app;
mod args;
mod editing;
mod keymap;
mod mouse;
mod query;
mod query_ui;
mod search;
mod status_bar;
mod syntax;
mod terminal;

use std::io::Write as _;
use std::process::ExitCode;

use app::{App, EditorOutcome};
use args::parse_args;

fn run() -> anyhow::Result<ExitCode> {
    let Some(cli_args) = parse_args(std::env::args())? else {
        return Ok(ExitCode::SUCCESS);
    };

    match App::new(cli_args).run()? {
        EditorOutcome::AcceptStdout(buffer) => {
            let mut stdout = std::io::stdout().lock();
            for chunk in buffer.chunks() {
                stdout.write_all(chunk.as_bytes())?;
            }
            stdout.flush()?;
            Ok(ExitCode::SUCCESS)
        }
        EditorOutcome::Exit(code) => Ok(code),
    }
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::from(1)
        }
    }
}
