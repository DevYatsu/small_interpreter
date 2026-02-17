use crate::{
    ast::{Expr, Statement},
    error::JitError,
    lexer::Token,
};
use logos::{Lexer, Logos};

pub struct Parser<'source> {
    lexer: Lexer<'source, Token<'source>>,
    line: usize,
    line_start: usize,
}

impl<'source> Parser<'source> {
    pub fn new(input: &'source str) -> Self {
        Self {
            lexer: Token::lexer(input),
            line: 1,
            line_start: 0,
        }
    }

    fn loc(&self) -> (usize, usize) {
        let col = self.lexer.span().start.saturating_sub(self.line_start) + 1;
        (self.line, col)
    }

    fn parse_var(&mut self, is_mut: bool) -> Result<Statement<'source>, JitError> {
        let (line, col) = self.loc();
        // We capture loc at start of var declaration (e.g. at 'le' or 'el' or 'name')??
        // actually 'parse_var' is called AFTER 'MutableVar/ImmutableVar' token is consumed.
        // So we are at Identifier.
        // The statement line/col should probably be the start of the statement.
        // But for simplicity, let's use the current location or passed location.
        // The `next` loop calls `parse_var` after matching `MutableVar`.
        // So `self.lexer.span()` corresponds to `MutableVar`.
        // Wait, `next` consumes the token.
        // So inside `next`, `self.lexer.span()` is the span of the token just matched (`MutableVar`).
        // So `self.loc()` gives location of `MutableVar`.

        let id = self.parse_id()?;
        self.parse_colon()?;
        let expr = self.parse_expr()?;

        if is_mut {
            Ok(Statement::MutVar(id, expr, (line, col)))
        } else {
            Ok(Statement::ImmutVar(id, expr, (line, col)))
        }
    }

    fn parse_id(&mut self) -> Result<&'source str, JitError> {
        let token = self.lexer.next();
        let (line, col) = self.loc(); // Location of the identifier

        match token {
            Some(Ok(Token::Identifier(id))) => Ok(id),
            Some(Ok(t)) => Err(JitError::Parsing(
                format!("Expected identifier, got '{:?}'", t),
                line,
                col,
            )),
            Some(Err(e)) => Err(JitError::Lexing(e, line, col)),
            None => Err(JitError::Parsing(
                "Expected identifier, got EOF".to_string(),
                line,
                col,
            )),
        }
    }

    fn parse_colon(&mut self) -> Result<(), JitError> {
        let token = self.lexer.next();
        let (line, col) = self.loc();

        match token {
            Some(Ok(Token::Colon)) => Ok(()),
            Some(Ok(t)) => Err(JitError::Parsing(
                format!("Expected ':', got '{:?}'", t),
                line,
                col,
            )),
            Some(Err(e)) => Err(JitError::Lexing(e, line, col)),
            None => Err(JitError::Parsing(
                "Expected ':', got EOF".to_string(),
                line,
                col,
            )),
        }
    }

    fn parse_expr(&mut self) -> Result<Expr<'source>, JitError> {
        let token = self.lexer.next();
        let (line, col) = self.loc();

        match token {
            Some(Ok(Token::Bool(b))) => Ok(Expr::Bool(b)),
            Some(Ok(Token::Number(n))) => Ok(Expr::Number(n)),
            Some(Ok(Token::String(s))) => Ok(Expr::String(s)),
            Some(Ok(Token::Identifier(i))) => Ok(Expr::Var(i)),
            Some(Ok(t)) => Err(JitError::Parsing(
                format!("Expected expression, got '{:?}'", t),
                line,
                col,
            )),
            Some(Err(e)) => Err(JitError::Lexing(e, line, col)),
            None => Err(JitError::Parsing(
                "Expected expression, got EOF".to_string(),
                line,
                col,
            )),
        }
    }
}

impl<'source> Iterator for Parser<'source> {
    type Item = Result<Statement<'source>, JitError>;

    fn next(&mut self) -> Option<Self::Item> {
        let token = self.lexer.next()?;
        let (line, col) = self.loc();

        match token {
            Ok(Token::Print) => {
                let expr_res = self.parse_expr();
                match expr_res {
                    Ok(expr) => Some(Ok(Statement::Print(expr, (line, col)))),
                    Err(e) => Some(Err(e)),
                }
            }
            Ok(Token::Colon) => Some(Err(JitError::Parsing(
                "Unexpected token ':'".to_string(),
                line,
                col,
            ))),
            Ok(Token::MutableVar) => Some(self.parse_var(true)),
            Ok(Token::ImmutableVar) => Some(self.parse_var(false)),
            Ok(Token::Bool(_))
            | Ok(Token::Number(_))
            | Ok(Token::String(_))
            | Ok(Token::Identifier(_)) => Some(Err(JitError::Parsing(
                format!(
                    "Unexpected token at start of statement: {:?}",
                    self.lexer.slice()
                ),
                line,
                col,
            ))),
            Ok(Token::Newline) => {
                self.line += 1;
                self.line_start = self.lexer.span().end;
                self.next()
            }
            Err(e) => Some(Err(JitError::Lexing(e, line, col))),
        }
    }
}
