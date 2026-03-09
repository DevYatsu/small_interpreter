use crate::backends::Backend;
use crate::error::JitError;
use crate::formatter;
use crate::parser;
use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Instant;

/// Format .pi files.
pub async fn run_fmt(files: Vec<String>) -> Result<(), JitError> {
    let mut to_format = Vec::new();
    if files.is_empty() {
        collect_files(Path::new("."), &mut to_format)?;
    } else {
        for file in files {
            let path = Path::new(&file);
            if path.is_dir() {
                collect_files(path, &mut to_format)?;
            } else if path.extension().map(|e| e == "pi").unwrap_or(false) {
                to_format.push(path.to_path_buf());
            }
        }
    }

    if to_format.is_empty() {
        return Ok(());
    }

    to_format
        .par_iter()
        .try_for_each(|path| formatter::format_file(path))?;

    Ok(())
}

fn collect_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), JitError> {
    if path.is_dir() {
        for entry in fs::read_dir(path).map_err(|e| JitError::runtime(e.to_string(), 0, 0))? {
            let entry = entry.map_err(|e| JitError::runtime(e.to_string(), 0, 0))?;
            let path = entry.path();
            if path.is_dir() {
                if path
                    .file_name()
                    .map(|n| n == "target" || n == ".git")
                    .unwrap_or(false)
                {
                    continue;
                }
                collect_files(&path, files)?;
            } else if path.extension().map(|e| e == "pi").unwrap_or(false) {
                files.push(path);
            }
        }
    } else if path.extension().map(|e| e == "pi").unwrap_or(false) {
        files.push(path.to_path_buf());
    }
    Ok(())
}

/// Run a `.pi` source file from disk.
pub async fn run_file(file_path: &str) -> Result<(), JitError> {
    let content = fs::read_to_string(file_path).map_err(|e| {
        JitError::runtime(format!("Failed to read file '{}': {}", file_path, e), 0, 0)
    })?;

    let program = match parser::Parser::new(&content).compile() {
        Ok(prog) => prog,
        Err(e) => {
            print_error(&content, &e);
            std::process::exit(1);
        }
    };

    if cfg!(debug_assertions) {
        println!("Starting execution…");
    }

    let start = Instant::now();
    let backend: Box<dyn Backend> = Box::new(crate::backends::interpreter::Interpreter);

    if let Err(e) = backend.run(program).await {
        print_error(&content, &e);
        std::process::exit(1);
    }

    if cfg!(debug_assertions) {
        println!("\nExecution completed in {:?}", start.elapsed());
    }

    Ok(())
}

/// Interactive REPL.
pub async fn run_repl() -> Result<(), JitError> {
    println!(
        "\x1b[1;36msmall_interpreter\x1b[0m {} — Pi language REPL",
        env!("CARGO_PKG_VERSION")
    );
    println!("Type \x1b[33mexit\x1b[0m or \x1b[33mquit\x1b[0m to leave, or press Ctrl-D.");
    println!();

    let mut rl = rustyline::DefaultEditor::new()
        .map_err(|e| JitError::runtime(format!("Failed to initialize REPL: {}", e), 0, 0))?;

    let history_file = ".pi_history";
    let _ = rl.load_history(history_file);

    let mut buffer = String::new();
    let mut continuation = false;

    loop {
        let prompt = if continuation {
            "\x1b[1;32m... \x1b[0m"
        } else {
            "\x1b[1;32m>>> \x1b[0m"
        };

        match rl.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
                if !continuation && matches!(trimmed, "exit" | "quit") {
                    println!("Bye!");
                    break;
                }

                buffer.push_str(trimmed);
                buffer.push('\n');

                let opens = buffer.chars().filter(|&c| c == '{').count();
                let closes = buffer.chars().filter(|&c| c == '}').count();
                continuation = opens > closes;

                if !continuation {
                    let src = buffer.trim().to_string();
                    if !src.is_empty() {
                        let _ = rl.add_history_entry(&src);
                        eval_and_print(src).await;
                    }
                    buffer.clear();
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => {
                println!("^C");
                buffer.clear();
                continuation = false;
            }
            Err(rustyline::error::ReadlineError::Eof) => {
                println!("Bye!");
                break;
            }
            Err(err) => {
                eprintln!("REPL error: {:?}", err);
                break;
            }
        }
    }

    let _ = rl.save_history(history_file);
    Ok(())
}

async fn eval_and_print(source: String) {
    let program = match parser::Parser::new(&source).compile() {
        Ok(prog) => prog,
        Err(e) => {
            print_error(&source, &e);
            return;
        }
    };

    let backend: Box<dyn Backend + Send> = Box::new(crate::backends::interpreter::Interpreter);
    if let Err(e) = backend.run(program).await {
        print_error(&source, &e);
    }
}

pub fn print_usage() {
    println!(
        "\
\x1b[1msmall_interpreter\x1b[0m — Pi scripting language

\x1b[1;33mUSAGE\x1b[0m
    small_interpreter [FILE]
    small_interpreter fmt [FILES...]

\x1b[1;33mARGS\x1b[0m
    FILE    Path to a .pi source file.  If omitted, starts the interactive REPL.
    FILES   Paths to .pi files or directories to format.

\x1b[1;33mFLAGS\x1b[0m
    -h, --help    Print this message and exit

\x1b[1;33mEXAMPLES\x1b[0m
    small_interpreter script.pi
    small_interpreter                 # interactive REPL
    small_interpreter fmt             # format current project
    small_interpreter fmt script.pi   # format a specific file
"
    );
}

pub fn print_error(source: &str, error: &JitError) {
    eprintln!("\x1b[31;1mError:\x1b[0m {}", error);
    let (line_num, col_num) = error.location();
    if line_num > 0 {
        let lines: Vec<&str> = source.lines().collect();
        if line_num <= lines.len() {
            let line_content = lines[line_num - 1];
            eprintln!("\n\x1b[34m{:>4} | \x1b[0m{}", line_num, line_content);
            let padding = " ".repeat(col_num.saturating_sub(1));
            eprintln!("\x1b[34m     | \x1b[0m{}\x1b[31;1m^\x1b[0m", padding);
        }
    }
}
