use std::path::Path;
use std::fs;
use std::time::Instant;

use ys_core::parser::Parser;
use ys_runtime::{run_interpreter, Interpreter};

/// Run a script file.
pub async fn run_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let source = fs::read_to_string(path)?;
    run_source(&source).await
}

/// Syntax-check a script file or directory.
pub async fn check_file(path: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if path.is_dir() {
        let mut count = 0;
        let mut errors = 0;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let p = entry.path();
            if p.extension().map_or(false, |e| e == "ys") {
                count += 1;
                if let Err(e) = check_source_internal(&fs::read_to_string(&p)?, Some(&p)).await {
                    errors += 1;
                    if let Some(je) = e.downcast_ref::<ys_core::error::JitError>() {
                        crate::error_display::display_error(je, &fs::read_to_string(&p)?);
                    } else {
                        eprintln!("{}: {}", p.display(), e);
                    }
                }
            }
        }
        if errors > 0 {
            return Err(format!("Checked {} files, found {} errors", count, errors).into());
        }
        println!("Checked {} files, no errors found.", count);
        Ok(())
    } else {
        let source = fs::read_to_string(path)?;
        check_source_internal(&source, Some(path)).await
    }
}

async fn check_source_internal(source: &str, path: Option<&Path>) -> Result<(), Box<dyn std::error::Error>> {
    let parser = Parser::new(source)?;
    let _program = parser.compile()?;
    if let Some(p) = path {
        println!("{}: OK", p.display());
    }
    Ok(())
}

/// Run source code directly.
pub async fn run_source(source: &str) -> Result<(), Box<dyn std::error::Error>> {
    let start = Instant::now();

    let parser = Parser::new(source)?;
    let program = parser.compile()?;
    
    let elapsed_compile = start.elapsed();
    
    let start_run = Instant::now();
    run_interpreter(program).await?;
    let elapsed_run = start_run.elapsed();

    println!("\n{} {:?} (compile: {:?}, run: {:?})", 
        "Done in", start.elapsed(), elapsed_compile, elapsed_run);
    
    Ok(())
}

/// Start an interactive REPL session.
pub async fn run_repl() -> Result<(), Box<dyn std::error::Error>> {
    let mut rl = rustyline::DefaultEditor::new()?;
    let _runtime = Interpreter;

    println!("YatsuScript REPL (press Ctrl-C to exit)");
    
    loop {
        let readline = rl.readline(">> ");
        match readline {
            Ok(line) => {
                rl.add_history_entry(line.as_str())?;
                
                if line.trim().is_empty() { continue; }

                let parser = match Parser::new(&line) {
                    Ok(p) => p,
                    Err(e) => {
                        crate::error_display::display_error(&e, &line);
                        continue;
                    }
                };

                let program = match parser.compile() {
                    Ok(p) => p,
                    Err(e) => {
                        crate::error_display::display_error(&e, &line);
                        continue;
                    }
                };

                if let Err(e) = run_interpreter(program).await {
                    crate::error_display::display_error(&e, &line);
                }
            }
            Err(rustyline::error::ReadlineError::Interrupted) => break,
            Err(rustyline::error::ReadlineError::Eof) => break,
            Err(err) => {
                println!("Error: {:?}", err);
                break;
            }
        }
    }
    
    Ok(())
}
