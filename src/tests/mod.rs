//! Unit and integration tests for the `yatsuscript` project.
//!
//! This module re-exports the test suites from the submodules:
//!
//! - [`test_lexer`] — lexical analysis (tokens, errors, ASCII-only validation).
//!
//! - [`test_parser`] — parser integration (source snippets → bytecode).
//!
//! - [`test_compiler`] — compiler internals (value representation, instruction set).
//!
//! These tests are run via `cargo test` and are not part of the runtime.

pub mod test_compiler;
pub mod test_lexer;
pub mod test_parser;
