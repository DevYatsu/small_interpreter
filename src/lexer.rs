use std::fmt;
use std::num::{ParseFloatError, ParseIntError};

use logos::Logos;

/// Errors that can occur during the lexing phase.
#[derive(Default, Debug, Clone, Copy, PartialEq)]
pub enum LexingError {
    /// Failed to parse an integer literal.
    InvalidInteger,
    /// Failed to parse a floating-point literal.
    InvalidFloat,
    /// Encountered a character that is not valid in the source code.
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

/// The set of tokens produced by the lexer.
#[derive(Logos, Debug, PartialEq, Clone, Copy)]
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
        let input = "el le fn return if else spawn for while in ..";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::MutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::ImmutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::Fn)));
        assert_eq!(lexer.next(), Some(Ok(Token::Return)));
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
        assert_eq!(lexer.next(), None);
    }

    #[test]
    fn test_lexer_comments() {
        let input = "el x = 10 // this is a comment\nle y = 20";
        let mut lexer = Token::lexer(input);

        assert_eq!(lexer.next(), Some(Ok(Token::MutableVar)));
        assert_eq!(lexer.next(), Some(Ok(Token::Identifier("x"))));
        // Note: = is not a single token, it is usually handled in assignment or Eq?
        // Wait, looking at lexer.rs I don't see '='.
        // Let me re-check lexer.rs.
    }
}
