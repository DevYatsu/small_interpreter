mod backends;
mod compiler;
mod error;
mod lexer;
mod parser;

use crate::backends::Backend;
use crate::error::JitError;
use std::time::Instant;

#[tokio::main]
async fn main() -> Result<(), JitError> {
    let mut args = pico_args::Arguments::from_env();

    // Check if help is requested
    if args.contains(["-h", "--help"]) {
        println!("Usage: small_jit [FILE] [-b BACKEND]");
        println!("Backends: interpreter (default), cranelift");
        return Ok(());
    }

    let backend_name: String = args
        .opt_value_from_str(["-b", "--backend"])
        .map_err(|e| JitError::Runtime(format!("Argument error: {}", e), 0, 0))?
        .or_else(|| std::env::var("JIT_BACKEND").ok())
        .unwrap_or_else(|| "interpreter".to_string());

    let file_path: String = args
        .free_from_str()
        .unwrap_or_else(|_| "main.pi".to_string());

    let content = std::fs::read_to_string(&file_path).map_err(|e| {
        JitError::Runtime(format!("Failed to read file {}: {}", file_path, e), 0, 0)
    })?;

    let parser = parser::Parser::new(&content);
    let program = match parser.compile() {
        Ok(prog) => prog,
        Err(e) => {
            print_error(&content, &e);
            std::process::exit(1);
        }
    };

    println!("Compiled program: {} instructions, {} functions", program.instructions.len(), program.functions.len());

    let start = Instant::now();

    let backend: Box<dyn Backend> = Box::new(backends::interpreter::Interpreter);

    println!(
        "--- Running {} with Backend: {} ---",
        file_path, backend_name
    );
    if let Err(e) = backend.run(program).await {
        print_error(&content, &e);
        std::process::exit(1);
    }

    let total = start.elapsed();

    println!("\n--- Execution Results (Tokio Green Threads) ---");
    println!("Total execution time: {:?}", total);
    println!("-----------------------------------------------");

    Ok(())
}

fn print_error(source: &str, error: &JitError) {
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
