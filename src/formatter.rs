use crate::error::JitError;
use crate::lexer::Token;
use logos::Logos;
use std::fs;
use std::path::Path;

pub fn format_file(path: &Path) -> Result<(), JitError> {
    let content = fs::read_to_string(path)
        .map_err(|e| JitError::runtime(format!("Failed to read file: {}", e), 0, 0))?;

    let formatted = format_source(&content)?;

    if content != formatted {
        fs::write(path, formatted)
            .map_err(|e| JitError::runtime(format!("Failed to write file: {}", e), 0, 0))?;
        println!("Formatted {}", path.display());
    }

    Ok(())
}

pub fn format_source(source: &str) -> Result<String, JitError> {
    let mut lexer = Token::lexer(source);
    let mut output = String::new();
    let mut indent_level: usize = 0;
    let mut at_start_of_line = true;
    let mut need_space = false;

    while let Some(token_result) = lexer.next() {
        let token = token_result.map_err(|e| JitError::Lexing {
            err: e,
            loc: crate::error::ErrorLoc::new(0, 0),
        })?;

        match token {
            Token::Newline => {
                output.push('\n');
                at_start_of_line = true;
                need_space = false;
                continue;
            }
            Token::RBrace => {
                indent_level = indent_level.saturating_sub(1);
            }
            _ => {}
        }

        if at_start_of_line {
            output.push_str(&"    ".repeat(indent_level));
            at_start_of_line = false;
            need_space = false;
        } else if need_space {
            match token {
                Token::Comma
                | Token::Colon
                | Token::LParen
                | Token::RParen
                | Token::LBracket
                | Token::RBracket
                | Token::Not
                | Token::Dot => {}
                _ => output.push(' '),
            }
        }

        match token {
            Token::LBrace => {
                output.push('{');
                indent_level += 1;
                need_space = true;
            }
            Token::RBrace => {
                output.push('}');
                need_space = true;
            }
            Token::LParen | Token::LBracket | Token::Dot => {
                output.push(match token {
                    Token::LParen => '(',
                    Token::LBracket => '[',
                    _ => '.',
                });
                need_space = false;
            }
            Token::RParen | Token::RBracket | Token::Comma | Token::Colon => {
                output.push(match token {
                    Token::RParen => ')',
                    Token::RBracket => ']',
                    Token::Comma => ',',
                    _ => ':',
                });
                need_space = true;
            }
            Token::Plus
            | Token::Minus
            | Token::Mul
            | Token::Div
            | Token::Eq
            | Token::Ne
            | Token::Lt
            | Token::Le
            | Token::Gt
            | Token::Ge => {
                if !output.ends_with(' ') {
                    output.push(' ');
                }
                output.push_str(lexer.slice());
                output.push(' ');
                need_space = false;
            }
            Token::Identifier(s) | Token::String(s) | Token::Template(s) => {
                if matches!(token, Token::String(_)) {
                    output.push('"');
                } else if matches!(token, Token::Template(_)) {
                    output.push('`');
                }
                output.push_str(s);
                if matches!(token, Token::String(_)) {
                    output.push('"');
                } else if matches!(token, Token::Template(_)) {
                    output.push('`');
                }
                need_space = true;
            }
            Token::Number(_) | Token::Bool(_) => {
                output.push_str(lexer.slice());
                need_space = true;
            }
            Token::MutableVar
            | Token::ImmutableVar
            | Token::Fn
            | Token::Return
            | Token::Continue
            | Token::If
            | Token::Else
            | Token::Spawn
            | Token::For
            | Token::While
            | Token::In => {
                output.push_str(lexer.slice());
                need_space = true;
            }
            Token::Range => {
                output.push_str("..");
                need_space = false;
            }
            Token::Not => {
                output.push('!');
                need_space = false;
            }
            Token::LineComment => {
                if !at_start_of_line && !output.ends_with(' ') && !output.ends_with('\n') {
                    output.push(' ');
                }
                output.push_str(lexer.slice());
            }
            Token::Newline => unreachable!(),
        }
    }

    Ok(output)
}
