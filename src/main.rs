//! # yatsuscript
//!
//! A fast bytecode interpreter for the **YatsuScript** language.

mod backends;
mod cli;
mod compiler;
mod error;
mod formatter;
mod lexer;
mod parser;

#[cfg(test)]
mod tests;

use crate::error::JitError;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<(), JitError> {
    let mut args = pico_args::Arguments::from_env();

    if args.contains(["-h", "--help"]) {
        cli::print_usage();
        return Ok(());
    }

    let subcommand = args
        .subcommand()
        .map_err(|e| JitError::runtime(format!("Failed to parse subcommand: {}", e), 0, 0))?;

    match subcommand.as_deref() {
        Some("fmt") => {
            let files: Vec<String> = args
                .finish()
                .into_iter()
                .map(|s| s.to_string_lossy().into_owned())
                .collect();
            cli::run_fmt(files).await
        }
        Some(file_path) => cli::run_file(file_path).await,
        None => cli::run_repl().await,
    }
}
