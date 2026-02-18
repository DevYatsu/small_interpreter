use std::fmt;
use std::num::{ParseFloatError, ParseIntError};

use logos::Logos;

#[derive(Default, Debug, Clone, PartialEq)]
pub enum LexingError {
    InvalidInteger(String),
    InvalidFloat(String),
    NonAsciiCharacter(char),
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

#[derive(Logos, Debug, PartialEq, Clone)]
#[logos(error(LexingError, LexingError::from_lexer))]
#[logos(skip r"[ \t\f]+")]
#[logos(skip r"/\*(?:[^*]|\*[^/])*\*/")]
pub enum Token<'source> {
    #[token("el")]
    MutableVar,
    #[token("le")]
    ImmutableVar,
    #[token("fn")]
    Fn,
    #[token("return")]
    Return,
    #[token("if")]
    If,
    #[token("else")]
    Else,
    #[token(":")]
    Colon,
    #[token("\n")]
    Newline,
    #[token("spawn")]
    Spawn,
    #[token("for")]
    For,
    #[token("while")]
    While,
    #[token("in")]
    In,
    #[token("..")]
    Range,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(",")]
    Comma,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Mul,
    #[token("/")]
    Div,
    #[token("==")]
    Eq,
    #[token("!=")]
    Ne,
    #[token("<")]
    Lt,
    #[token("<=")]
    Le,
    #[token(">")]
    Gt,
    #[token(">=")]
    Ge,
    #[token("false", |_| false)]
    #[token("true", |_| true)]
    Bool(bool),

    #[regex(
        r"-?(?:0|[1-9]\d*)(?:_\d+)*(?:\.(?:\d+(?:_\d+)*))?(?:[eE][+-]?\d+(?:_\d+)*)?",
        |lex| lex.slice().replace("_", "").parse::<f64>()
    )]
    Number(f64),

    #[regex(r#""([^"\\\x00-\x1F]|\\(["\\bnfrt/]|u[a-fA-F0-9]{4}))*""#, |lex| {
        let s = lex.slice();
        &s[1..s.len()-1]
    })]
    String(&'source str),

    #[regex(r"[[:alpha:]_][[:alnum:]_]*", |lex| lex.slice())]
    Identifier(&'source str),

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
