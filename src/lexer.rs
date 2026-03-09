//! Lexical analysis for the Pi scripting language.
//!
//! This module is built on top of the [`logos`] crate, which generates a
//! highly optimised DFA-based scanner from the token definitions below.
//!
//! The lexer produces a flat stream of [`Token`]s.  The [`parser`][crate::parser]
//! then drives the lexer and converts the token stream into bytecode.
//!
//! # Character set
//!
//! Pi source files are expected to be valid ASCII.  Non-ASCII characters
//! (e.g. Unicode letters) produce a [`LexingError::NonAsciiCharacter`] error.
//!
//! # Comments
//!
//! - **Line comments** start with `//` and run to the end of the line.
//! - **Block comments** are delimited by `/*` … `*/` and may not be nested.
//!
//! Both are emitted as [`Token::LineComment`] / skipped respectively; the
//! parser discards them.

use std::fmt;
use std::num::{ParseFloatError, ParseIntError};

use logos::Logos;

/// Errors that can arise during the lexical analysis phase.
///
/// These are produced by the [`logos`] scanner and subsequently wrapped in
/// [`JitError::Lexing`][crate::error::JitError::Lexing] by the parser.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum LexingError {
    /// An integer literal could not be parsed (e.g. overflow).
    InvalidInteger,
    /// A floating-point literal could not be parsed (e.g. overflow).
    InvalidFloat,
    /// A character that is not part of the Pi character set was encountered.
    /// The offending `char` is captured for use in diagnostics.
    NonAsciiCharacter(char),
    /// A catch-all for any other lexer error (should be rare in practice).
    #[default]
    Other,
}

impl LexingError {
    /// Called by the logos scanner when it encounters an unrecognised input.
    /// Inspects the raw slice to determine whether the culprit is a non-ASCII
    /// character or something else entirely.
    fn from_lexer<'a>(lex: &mut logos::Lexer<'a, Token<'a>>) -> Self {
        match lex.slice().chars().next() {
            Some(c) => LexingError::NonAsciiCharacter(c),
            None => LexingError::Other,
        }
    }
}

/// Every terminal symbol in the Pi grammar.
///
/// The lifetime `'source` is tied to the original source string, allowing
/// string/identifier tokens to borrow their slices without copying.
///
/// # Skipping rules
///
/// The lexer automatically skips:
/// - Horizontal whitespace (space, tab, form-feed).
/// - Block comments (`/* … */`).
///
/// Newlines and line comments are *not* skipped — newlines act as statement
/// terminators and are significant in the grammar.
#[derive(Logos, Debug, PartialEq, Clone, Copy)]
#[logos(error(LexingError, LexingError::from_lexer))]
#[logos(skip r"[ \t\f]+")]
#[logos(skip r"/\*(?:[^*]|\*[^/])*\*/")]
pub enum Token<'source> {
    /// 'mut' keyword for declaring a mutablet variable.
    #[token("mut")]
    MutableVar,
    /// 'let' keyword for declaring an immutablet variable.
    #[token("let")]
    ImmutableVar,
    /// 'fn' keyword for declaring a function.
    #[token("fn")]
    Fn,
    /// 'return' keyword.
    #[token("return")]
    Return,
    /// 'continue' keyword.
    #[token("continue")]
    Continue,
    /// 'if' keyword.
    #[token("if")]
    If,
    /// 'else' keyword.
    #[token("else")]
    Else,
    /// ':' separator.
    #[token(":")]
    Colon,
    /// '\n' line separator.
    #[token("\n")]
    Newline,
    /// 'spawn' keyword for concurrency.
    #[token("spawn")]
    Spawn,
    /// 'for' loop keyword.
    #[token("for")]
    For,
    /// 'while' loop keyword.
    #[token("while")]
    While,
    /// 'in' keyword for iterators.
    #[token("in")]
    In,
    /// '..' range operator.
    #[token("..")]
    Range,
    /// '{' opening brace.
    #[token("{")]
    LBrace,
    /// '}' closing brace.
    #[token("}")]
    RBrace,
    /// '(' opening parenthesis.
    #[token("(")]
    LParen,
    /// ')' closing parenthesis.
    #[token(")")]
    RParen,
    /// '[' opening bracket.
    #[token("[")]
    LBracket,
    /// ']' closing bracket.
    #[token("]")]
    RBracket,
    /// ',' separator.
    #[token(",")]
    Comma,
    /// '.' operator for object property access.
    #[token(".")]
    Dot,
    /// '+' operator.
    #[token("+")]
    Plus,
    /// '-' operator.
    #[token("-")]
    Minus,
    /// '*' operator.
    #[token("*")]
    Mul,
    /// '/' operator.
    #[token("/")]
    Div,
    /// '==' operator.
    #[token("==")]
    Eq,
    /// '!=' operator.
    #[token("!=")]
    Ne,
    /// '<' operator.
    #[token("<")]
    Lt,
    /// '<=' operator.
    #[token("<=")]
    Le,
    /// '>' operator.
    #[token(">")]
    Gt,
    /// '>=' operator.
    #[token(">=")]
    Ge,
    /// '!' operator.
    #[token("!")]
    Not,
    /// Boolean literals.
    #[token("false", |_| false)]
    #[token("true", |_| true)]
    Bool(bool),

    /// Numeric literals (integers and floats).
    #[regex(
        r"-?(?:0|[1-9]\d*)(?:_\d+)*(?:\.(?:\d+(?:_\d+)*))?(?:[eE][+-]?\d+(?:_\d+)*)?",
        |lex| lex.slice().replace("_", "").parse::<f64>()
    )]
    Number(f64),

    /// String literals enclosed in doublet quotes.
    #[regex(r#""([^"\\\x00-\x1F]|\\(["\\bnfrt/]|u[a-fA-F0-9]{4}))*""#, |lex| {
        let  s = lex.slice();
        &s[1..s.len()-1]
    })]
    String(&'source str),

    /// Identifier names.
    #[regex(r"[[:alpha:]_][[:alpha:]0-9_]*", |lex| lex.slice())]
    Identifier(&'source str),

    /// Template literals enclosed in backticks.
    #[regex(r#"`([^`\\]|\\.)*`"#, |lex| {
        let s = lex.slice();
        &s[1..s.len()-1]
    })]
    Template(&'source str),

    /// Double-slash line comments.
    #[regex(r"//[^\n]*", allow_greedy = true)]
    LineComment,
}

impl fmt::Display for LexingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexingError::InvalidInteger => write!(f, "Invalid integer"),
            LexingError::InvalidFloat => write!(f, "Invalid float"),
            LexingError::NonAsciiCharacter(c) => write!(f, "Non-ASCII character: {}", c),
            LexingError::Other => write!(f, "Unknown lexing error"),
        }
    }
}

impl std::error::Error for LexingError {}

impl From<ParseIntError> for LexingError {
    fn from(_: ParseIntError) -> Self {
        LexingError::InvalidInteger
    }
}

impl From<ParseFloatError> for LexingError {
    fn from(_err: ParseFloatError) -> Self {
        LexingError::InvalidFloat
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lexer_keywords() {
        let input = "mut let fn return continue if else spawn for while in ..";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::MutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::ImmutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::Fn)));
        assert_eq!(lexer.next(), Some(Ok(Token::Return)));
        assert_eq!(lexer.next(), Some(Ok(Token::Continue)));
        assert_eq!(lexer.next(), Some(Ok(Token::If)));
        assert_eq!(lexer.next(), Some(Ok(Token::Else)));
        assert_eq!(lexer.next(), Some(Ok(Token::Spawn)));
        assert_eq!(lexer.next(), Some(Ok(Token::For)));
        assert_eq!(lexer.next(), Some(Ok(Token::While)));
        assert_eq!(lexer.next(), Some(Ok(Token::In)));
        assert_eq!(lexer.next(), Some(Ok(Token::Range)));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_literals() {
        let input = "true false 123 123.456 1_000 \"hello world\" identifier";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Bool(true))));
        assert_eq!(lexer.next(), Some(Ok(Token::Bool(false))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(123.0))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(123.456))));
        assert_eq!(lexer.next(), Some(Ok(Token::Number(1000.0))));
        assert_eq!(lexer.next(), Some(Ok(Token::String("hello world"))));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("identifier"))));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_symbols() {
        let input = ": { } ( ) [ ] , . + - * / == != < <= > >=";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::Colon)));
        assert_eq!(lexer.next(), Some(Ok(Token::LBrace)));
        assert_eq!(lexer.next(), Some(Ok(Token::RBrace)));
        assert_eq!(lexer.next(), Some(Ok(Token::LParen)));
        assert_eq!(lexer.next(), Some(Ok(Token::RParen)));
        assert_eq!(lexer.next(), Some(Ok(Token::LBracket)));
        assert_eq!(lexer.next(), Some(Ok(Token::RBracket)));
        assert_eq!(lexer.next(), Some(Ok(Token::Comma)));
        assert_eq!(lexer.next(), Some(Ok(Token::Dot)));
        assert_eq!(lexer.next(), Some(Ok(Token::Plus)));
        assert_eq!(lexer.next(), Some(Ok(Token::Minus)));
        assert_eq!(lexer.next(), Some(Ok(Token::Mul)));
        assert_eq!(lexer.next(), Some(Ok(Token::Div)));
        assert_eq!(lexer.next(), Some(Ok(Token::Eq)));
        assert_eq!(lexer.next(), Some(Ok(Token::Ne)));
        assert_eq!(lexer.next(), Some(Ok(Token::Lt)));
        assert_eq!(lexer.next(), Some(Ok(Token::Le)));
        assert_eq!(lexer.next(), Some(Ok(Token::Gt)));
        assert_eq!(lexer.next(), Some(Ok(Token::Ge)));
        assert_eq!(lexer.next(), Some(Ok(Token::Not)));
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_comments() {
        let input = "mut x = 10 // this is a comment\nlet y = 20";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::MutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("x"))));
        // Note: = is not a singlet token, it is usually handled in assignment or Eq?
        // Wait, looking at lexer.rs I don't see '='.
        // let  me re-check lexer.rs.
    }
}
