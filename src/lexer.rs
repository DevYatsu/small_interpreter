use std::fmt;
use std::num::{ParseFloatError, ParseIntError};

use logos::Logos;

/// Errors that can occur during the lexing phase.
#[derive(Default, Debug, Clone, PartialEq)]
pub enum LexingError {
    /// Failed to parse an integer literal.
    InvalidInteger(String),
    /// Failed to parse a floating-point literal.
    InvalidFloat(String),
    /// Encountered a character that is not valid in the source code.
    NonAsciiCharacter(char),
    /// General catch-all for other lexing errors.
    #[default]
    Other,
}

impl LexingError {
    fn from_lexer<'a>(lex: &mut logos::Lexer<'a, Token<'a>>) -> Self {
        match lex.slice().chars().next() {
            Some(c) => LexingError::NonAsciiCharacter(c),
            None => LexingError::Other,
        }
    }
}

/// The set of tokens produced by the lexer.
#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(error(LexingError, LexingError::from_lexer))]
#[logos(skip r"[ \t\f]+")]
#[logos(skip r"/\*(?:[^*]|\*[^/])*\*/")]
pub enum Token<'source> {
    /// 'el' keyword for declaring a mutable variable.
    #[token("el")]
    MutableVar,
    /// 'le' keyword for declaring an immutable variable.
    #[token("le")]
    ImmutableVar,
    /// 'fn' keyword for declaring a function.
    #[token("fn")]
    Fn,
    /// 'return' keyword.
    #[token("return")]
    Return,
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

    /// String literals enclosed in double quotes.
    #[regex(r#""([^"\\\x00-\x1F]|\\(["\\bnfrt/]|u[a-fA-F0-9]{4}))*""#, |lex| {
        let s = lex.slice();
        &s[1..s.len()-1]
    })]
    String(&'source str),

    /// Identifier names.
    #[regex(r"[[:alpha:]_][[:alpha:]0-9_]*", |lex| lex.slice())]
    Identifier(&'source str),

    /// Double-slash line comments.
    #[regex(r"//[^\n]*", allow_greedy = true)]
    LineComment,
}

impl fmt::Display for LexingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LexingError::InvalidInteger(s) => write!(f, "Invalid integer: {}", s),
            LexingError::InvalidFloat(s) => write!(f, "Invalid float: {}", s),
            LexingError::NonAsciiCharacter(c) => write!(f, "Non-ASCII character: {}", c),
            LexingError::Other => write!(f, "Unknown lexing error"),
        }
    }
}

impl std::error::Error for LexingError {}

impl From<ParseIntError> for LexingError {
    fn from(err: ParseIntError) -> Self {
        use std::num::IntErrorKind::*;
        match err.kind() {
            PosOverflow | NegOverflow => LexingError::InvalidInteger("overflow error".to_owned()),
            _ => LexingError::InvalidInteger("other error".to_owned()),
        }
    }
}

impl From<ParseFloatError> for LexingError {
    fn from(_err: ParseFloatError) -> Self {
        LexingError::InvalidFloat("float parse error".to_owned())
    }
}
