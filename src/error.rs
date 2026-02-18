use crate::lexer::LexingError;
use thiserror::Error;

/// Errors that can occur during compilation or execution of a program.
#[derive(Error, Debug, Clone)]
pub enum JitError {
    /// Error encountered during the lexical analysis phase.
    #[error("Lexing error at {1}:{2}:  {0:?}")]
    Lexing(LexingError, usize, usize),
    /// Error encountered during the parsing or bytecode generation phase.
    #[error("Parsing error at {1}:{2}:  {0}")]
    Parsing(String, usize, usize),
    /// Error encountered during the execution of the bytecode.
    #[error("Runtime error at {1}:{2}:  {0}")]
    Runtime(String, usize, usize),
    /// Attempted to access a variable that has not been defined.
    #[error("Unknown variable at {1}:{2}:  {0}")]
    UnknownVariable(String, usize, usize),
    /// Attempted to re-assign an immutable variable ('le') or redefine it.
    #[error("Redefinition of immutable variable at {1}:{2}: '{0}' was already defined on line {3}")]
    RedefinitionOfImmutableVariable(String, usize, usize, usize),
}

impl JitError {
    pub fn location(&self) -> (usize, usize) {
        match self {
            JitError::Lexing(_, line, col) => (*line, *col),
            JitError::Parsing(_, line, col) => (*line, *col),
            JitError::Runtime(_, line, col) => (*line, *col),
            JitError::UnknownVariable(_, line, col) => (*line, *col),
            JitError::RedefinitionOfImmutableVariable(_, line, col, _) => (*line, *col),
        }
    }
}
