mod ast;
mod compiler;
mod error;
mod parser;
mod lexer;
mod vm;

use std::time::Instant;
use crate::error::JitError;

fn main() -> Result<(), JitError> {
    let content = std::fs::read_to_string("./main.pi")
        .map_err(|e| JitError::Runtime(format!("Failed to read file: {}", e), 0, 0))?
        .repeat(10000);

    let parser = parser::Parser::new(&content);
    let compiled_program = match compiler::compile(parser) {
        Ok(prog) => prog,
        Err(e) => {
            print_error(&content, &e);
            std::process::exit(1);
        }
    };

    let start = Instant::now();
    if let Err(e) = vm::run(compiled_program) {
        print_error(&content, &e);
        std::process::exit(1);
    }
    let total = start.elapsed();

    println!("total: {:?}, avg: {:?}", total, total);

    Ok(())
}

fn print_error(source: &str, error: &JitError) {
    eprintln!("\x1b[31mError:\x1b[0m {}", error); // Red "Error:"
    let (line_num, col_num) = error.location();
    if line_num > 0 {
        let lines: Vec<&str> = source.lines().collect();
        if line_num <= lines.len() {
            let line_content = lines[line_num - 1];
            eprintln!("\n\x1b[34m{:>4} | \x1b[0m{}", line_num, line_content); // Blue line number
            let padding = " ".repeat(col_num.saturating_sub(1));
            eprintln!("\x1b[34m     | \x1b[0m{}\x1b[31m^\x1b[0m", padding); // Red caret
        }
    }
}
