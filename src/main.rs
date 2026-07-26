use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use romero::ProgressEvent;

mod presentation;

use presentation::verbose_only;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Show cache activity and per-file promotion details.
    #[arg(long)]
    verbose: bool,

    /// Romero root directory. Defaults to the current directory.
    root: Option<PathBuf>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = match cli.root {
        Some(root) => root,
        None => match std::env::current_dir() {
            Ok(root) => root,
            Err(error) => {
                eprintln!("romero: cannot determine the current directory: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    match romero::run_with_progress(&root, |event| {
        if cli.verbose || !verbose_only(event) {
            match event {
                ProgressEvent::Incomplete { detail } => {
                    let mut stderr = anstream::stderr();
                    let _ = write!(stderr, "{}", detail.colored());
                }
                ProgressEvent::Duplicate { detail } => {
                    let mut stderr = anstream::stderr();
                    let _ = write!(stderr, "{}", detail.colored());
                }
                _ => eprintln!("{event}"),
            }
        }
    }) {
        Ok(summary) => {
            let mut stdout = anstream::stdout();
            match write!(stdout, "{}", summary.colored()) {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("romero: cannot write summary: {error}");
                    ExitCode::FAILURE
                }
            }
        }
        Err(error) => {
            eprintln!("romero: {error}");
            ExitCode::FAILURE
        }
    }
}
