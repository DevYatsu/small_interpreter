use crate::{
    compiler::{Instruction, Loc, Program, UserFunction, Value},
    error::JitError,
    lexer::Token,
};
use logos::{Lexer, Logos, Span};
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::Arc;

#[derive(Clone, Copy)]
struct VarInfo {
    idx: usize,
    is_mut: bool,
    is_global: bool,
    first_line: usize,
}

enum Accessor {
    Index(usize),
    Field(u32),
}

/// The Parser transforms source code into a compiled Program (bytecode).
pub struct Parser<'source> {
    lexer: Lexer<'source, Token<'source>>,
    /// Lookahead buffer for up to 3 tokens.
    peeked: [Option<(Token<'source>, Span)>; 3],

    line: usize,
    line_start: usize,

    /// Global variables.
    globals: FxHashMap<&'source str, VarInfo>,
    /// Local variables in the current function/scope.
    locals: FxHashMap<&'source str, VarInfo>,

    /// Interned strings.
    strings: Vec<Arc<str>>,
    /// Map for fast interning lookups.
    string_map: FxHashMap<Arc<str>, u32>,

    functions: Vec<UserFunction>,

    next_reg: usize,
    next_global: usize,

    is_in_spawn: bool,
    is_in_function: bool,
    captures_stack: Vec<FxHashSet<usize>>,
    spawn_start_regs: Vec<usize>,
}

impl<'source> Parser<'source> {
    pub fn new(input: &'source str) -> Self {
        let mut lexer = Token::lexer(input);
        let mut p = Self {
            peeked: [None, None, None],
            lexer,
            line: 1,
            line_start: 0,
            globals: FxHashMap::default(),
            locals: FxHashMap::default(),
            strings: Vec::with_capacity(64),
            string_map: FxHashMap::default(),
            next_reg: 0,
            next_global: 0,
            is_in_spawn: false,
            is_in_function: false,
            captures_stack: Vec::new(),
            spawn_start_regs: Vec::new(),
            functions: Vec::with_capacity(16),
        };
        // Fill initial lookahead
        p.advance().ok();
        p.advance().ok();
        p.advance().ok();
        p
    }

    /// Advance the lexer and return the previous token and its span.
    fn advance(&mut self) -> Result<(Token<'source>, Span), JitError> {
        let res = self.peeked[0].take();
        self.peeked[0] = self.peeked[1].take();
        self.peeked[1] = self.peeked[2].take();
        self.peeked[2] = match self.lexer.next() {
            Some(Ok(t)) => Some((t, self.lexer.span())),
            Some(Err(e)) => return Err(JitError::Lexing(e, self.line, self.loc().col as usize)),
            None => None,
        };

        match res {
            Some((t, s)) => {
                if let Token::Newline = t {
                    self.line += 1;
                    self.line_start = s.end;
                }
                Ok((t, s))
            }
            None => Err(JitError::Parsing(
                "Unexpected EOF".into(),
                self.line,
                self.loc().col as usize,
            )),
        }
    }

    #[inline(always)]
    fn peek(&self) -> Option<Token<'source>> {
        self.peeked[0].as_ref().map(|(t, _)| *t)
    }

    #[inline(always)]
    fn peek_n(&self, n: usize) -> Option<Token<'source>> {
        self.peeked[n].as_ref().map(|(t, _)| *t)
    }

    #[inline(always)]
    fn loc(&self) -> Loc {
        let span = self.peeked[0]
            .as_ref()
            .map(|(_, s)| s.clone())
            .unwrap_or_else(|| self.lexer.span());
        let col = span.start.saturating_sub(self.line_start) + 1;
        Loc {
            line: self.line as u32,
            col: col as u32,
        }
    }

    pub fn compile(mut self) -> Result<Program, JitError> {
        let mut instructions = Vec::new();
        while self.peek().is_some() {
            if let Some(res) = self.parse_statement(&mut instructions) {
                res?;
            } else {
                break;
            }
        }
        Ok(Program {
            instructions: Arc::from(instructions),
            functions: Arc::from(self.functions),
            string_pool: Arc::from(self.strings),
            locals_count: self.next_reg,
            globals_count: self.next_global,
        })
    }

    fn intern(&mut self, s: &str) -> u32 {
        if let Some(&id) = self.string_map.get(s) {
            id
        } else {
            let id = self.strings.len() as u32;
            let arc_s: Arc<str> = Arc::from(s);
            self.strings.push(arc_s.clone());
            self.string_map.insert(arc_s, id);
            id
        }
    }

    fn alloc_reg(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn skip_newlines(&mut self) {
        while matches!(self.peek(), Some(Token::Newline) | Some(Token::LineComment)) {
            self.advance().ok();
        }
    }

    fn expect(&mut self, expected: Token) -> Result<(), JitError> {
        let (t, s) = self.advance()?;
        if t == expected {
            Ok(())
        } else {
            let col = s.start.saturating_sub(self.line_start) + 1;
            Err(JitError::Parsing(
                format!("Expected {:?}", expected),
                self.line,
                col,
            ))
        }
    }

    fn parse_expr(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        self.parse_binary(0, instructions)
    }

    fn parse_binary(
        &mut self,
        min_prec: u8,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut lhs = self.parse_primary(instructions)?;
        loop {
            let op = match self.peek() {
                Some(t) => t,
                _ => break,
            };
            let prec = match op {
                Token::Eq | Token::Ne => 1,
                Token::Lt | Token::Le | Token::Gt | Token::Ge => 2,
                Token::Plus | Token::Minus => 3,
                Token::Mul | Token::Div => 4,
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            self.advance()?;
            let loc = self.loc();
            let rhs = self.parse_binary(prec + 1, instructions)?;
            let dst = self.alloc_reg();
            let instr = match op {
                Token::Eq => Instruction::Eq { dst, lhs, rhs },
                Token::Ne => Instruction::Ne { dst, lhs, rhs },
                Token::Lt => Instruction::Lt { dst, lhs, rhs, loc },
                Token::Le => Instruction::Le { dst, lhs, rhs, loc },
                Token::Gt => Instruction::Gt { dst, lhs, rhs, loc },
                Token::Ge => Instruction::Ge { dst, lhs, rhs, loc },
                Token::Plus => Instruction::Add { dst, lhs, rhs, loc },
                Token::Minus => Instruction::Sub { dst, lhs, rhs, loc },
                Token::Mul => Instruction::Mul { dst, lhs, rhs, loc },
                Token::Div => Instruction::Div { dst, lhs, rhs, loc },
                _ => unreachable!(),
            };
            instructions.push(instr);
            lhs = dst;
        }
        Ok(lhs)
    }

    fn parse_primary(&mut self, instructions: &mut Vec<Instruction>) -> Result<usize, JitError> {
        let loc = self.loc();
        let (token, _span) = self.advance()?;
        let mut current_reg = match token {
            Token::LParen => {
                let r = self.parse_expr(instructions)?;
                self.expect(Token::RParen)?;
                Ok(r)
            }
            Token::Number(n) => {
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::number(n),
                });
                Ok(r)
            }
            Token::Bool(b) => {
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral {
                    dst: r,
                    val: Value::bool(b),
                });
                Ok(r)
            }
            Token::String(s) => {
                let unescaped = unescape_string(s);
                let val = Value::sso(&unescaped)
                    .unwrap_or_else(|| Value::object(self.intern(&unescaped)));
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral { dst: r, val });
                Ok(r)
            }
            Token::LBracket => self.parse_list_literal(instructions),
            Token::LBrace => self.parse_object_literal(instructions),
            Token::Identifier(id) => {
                if matches!(self.peek(), Some(Token::LParen)) {
                    self.advance()?; // consume (
                    let args = self.parse_call_args(instructions)?;
                    let dst = self.alloc_reg();

                    if let Some(info) = self.get_var(id) {
                        let callee_reg = self.load_var(info, instructions);
                        instructions.push(Instruction::CallDynamic {
                            callee_reg,
                            args_regs: Arc::from(args),
                            dst: Some(dst),
                            loc: self.loc(),
                        });
                    } else {
                        let name_id = self.intern(id);
                        instructions.push(Instruction::Call {
                            name_id,
                            args_regs: Arc::from(args),
                            dst: Some(dst),
                            loc: self.loc(),
                        });
                    }
                    Ok(dst)
                } else if let Some(info) = self.get_var(id) {
                    Ok(self.load_var(info, instructions))
                } else {
                    // Potential function literal
                    let val = Value::sso(id).unwrap_or_else(|| Value::object(self.intern(id)));
                    let r = self.alloc_reg();
                    instructions.push(Instruction::LoadLiteral { dst: r, val });
                    Ok(r)
                }
            }
            _ => Err(JitError::Parsing(
                "Expected expression".into(),
                loc.line as usize,
                loc.col as usize,
            )),
        }?;

        // Handle suffixes:indexing [expr] and property access .id
        loop {
            self.skip_newlines();
            match self.peek() {
                Some(Token::LBracket) => {
                    self.advance()?;
                    let index_reg = self.parse_expr(instructions)?;
                    self.expect(Token::RBracket)?;
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::ListGet {
                        dst,
                        list: current_reg,
                        index_reg,
                        loc: self.loc(),
                    });
                    current_reg = dst;
                }
                Some(Token::Dot) => {
                    self.advance()?;
                    let id = match self.advance()? {
                        (Token::Identifier(id), _) => id,
                        _ => {
                            return Err(JitError::Parsing(
                                "Expected property name after '.'".into(),
                                self.line,
                                0,
                            ));
                        }
                    };
                    let name_id = self.intern(id);
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::ObjectGet {
                        dst,
                        obj: current_reg,
                        name_id,
                        loc: self.loc(),
                    });
                    current_reg = dst;
                }
                _ => break,
            }
        }
        Ok(current_reg)
    }

    fn parse_call_args(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<Vec<usize>, JitError> {
        let mut args = Vec::new();
        self.skip_newlines();
        if !matches!(self.peek(), Some(Token::RParen)) {
            loop {
                self.skip_newlines();
                args.push(self.parse_expr(instructions)?);
                self.skip_newlines();
                match self.peek() {
                    Some(Token::Comma) => {
                        self.advance()?;
                        if matches!(self.peek(), Some(Token::RParen)) {
                            break;
                        }
                    }
                    _ => break,
                }
            }
        }
        self.expect(Token::RParen)?;
        Ok(args)
    }

    fn parse_list_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut elements = Vec::new();
        self.skip_newlines();
        if !matches!(self.peek(), Some(Token::RBracket)) {
            loop {
                self.skip_newlines();
                elements.push(self.parse_expr(instructions)?);
                self.skip_newlines();
                if matches!(self.peek(), Some(Token::Comma)) {
                    self.advance()?;
                    if matches!(self.peek(), Some(Token::RBracket)) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RBracket)?;

        let dst = self.alloc_reg();
        instructions.push(Instruction::NewList {
            dst,
            len: elements.len(),
        });
        for (i, &src) in elements.iter().enumerate() {
            let index_reg = self.alloc_reg();
            instructions.push(Instruction::LoadLiteral {
                dst: index_reg,
                val: Value::number(i as f64),
            });
            instructions.push(Instruction::ListSet {
                list: dst,
                index_reg,
                src,
                loc: self.loc(),
            });
        }
        Ok(dst)
    }

    fn parse_object_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut fields = Vec::with_capacity(4);
        self.skip_newlines();
        if !matches!(self.peek(), Some(Token::RBrace)) {
            loop {
                self.skip_newlines();
                let name = match self.advance()? {
                    (Token::Identifier(id), _) => id,
                    _ => {
                        return Err(JitError::Parsing(
                            "Expected field name".into(),
                            self.line,
                            0,
                        ));
                    }
                };
                self.expect(Token::Colon)?;
                let val_reg = self.parse_expr(instructions)?;
                fields.push((name, val_reg));
                self.skip_newlines();
                if matches!(self.peek(), Some(Token::Comma)) {
                    self.advance()?;
                    if matches!(self.peek(), Some(Token::RBrace)) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RBrace)?;

        let dst = self.alloc_reg();
        instructions.push(Instruction::NewObject {
            dst,
            capacity: fields.len(),
        });
        for (name, src) in fields {
            let name_id = self.intern(name);
            instructions.push(Instruction::ObjectSet {
                obj: dst,
                name_id,
                src,
                loc: self.loc(),
            });
        }
        Ok(dst)
    }

    fn parse_statement(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Option<Result<(), JitError>> {
        let token = match self.peek() {
            Some(t) => t,
            None => return None,
        };

        match token {
            Token::Newline | Token::LineComment => {
                self.advance().ok();
                self.parse_statement(instructions)
            }
            Token::MutableVar => Some(self.parse_var_decl(true, instructions)),
            Token::ImmutableVar => Some(self.parse_var_decl(false, instructions)),
            Token::For => Some(self.parse_for_loop(instructions)),
            Token::While => {
                self.advance().ok();
                Some(self.parse_while_loop(instructions))
            }
            Token::Fn => {
                self.advance().ok();
                Some(self.parse_fn_decl())
            }
            Token::If => {
                self.advance().ok();
                Some(self.parse_if_stmt(instructions))
            }
            Token::Return => {
                self.advance().ok();
                Some(self.parse_return_stmt(instructions))
            }
            Token::Spawn => {
                self.advance().ok();
                Some(self.parse_spawn_stmt(instructions))
            }
            Token::Identifier(id) => {
                self.advance().ok();
                Some(self.parse_id_stmt(id, instructions))
            }
            Token::RBrace => None,
            _ => Some(Err(JitError::Parsing(
                format!("Unexpected token {:?}", token),
                self.line,
                self.loc().col as usize,
            ))),
        }
    }

    fn parse_id_stmt(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        match self.peek() {
            Some(Token::Colon) | Some(Token::LBracket) | Some(Token::Dot) => {
                self.parse_assignment(id, instructions)
            }
            _ => self.parse_call_stmt(id, instructions),
        }
    }

    fn parse_call_stmt(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let args = if matches!(self.peek(), Some(Token::LParen)) {
            self.advance()?;
            self.parse_call_args(instructions)?
        } else {
            let mut args = Vec::new();
            while let Some(t) = self.peek() {
                if matches!(
                    t,
                    Token::Newline | Token::RBrace | Token::RParen | Token::RBracket | Token::Comma
                ) {
                    break;
                }
                args.push(self.parse_expr(instructions)?);
                if matches!(self.peek(), Some(Token::Comma)) {
                    self.advance()?;
                } else {
                    break;
                }
            }
            args
        };

        if let Some(info) = self.get_var(id) {
            let callee_reg = self.load_var(info, instructions);
            instructions.push(Instruction::CallDynamic {
                callee_reg,
                args_regs: Arc::from(args),
                dst: None,
                loc: self.loc(),
            });
        } else {
            let name_id = self.intern(id);
            instructions.push(Instruction::Call {
                name_id,
                args_regs: Arc::from(args),
                dst: None,
                loc: self.loc(),
            });
        }
        Ok(())
    }

    fn parse_assignment(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let loc = self.loc();
        let info = self.get_var(id).ok_or_else(|| {
            JitError::UnknownVariable(id.into(), loc.line as usize, loc.col as usize)
        })?;

        let mut accessors = Vec::new();
        loop {
            match self.peek() {
                Some(Token::LBracket) => {
                    self.advance()?;
                    accessors.push(Accessor::Index(self.parse_expr(instructions)?));
                    self.expect(Token::RBracket)?;
                }
                Some(Token::Dot) => {
                    self.advance()?;
                    match self.advance()? {
                        (Token::Identifier(field), _) => {
                            accessors.push(Accessor::Field(self.intern(field)))
                        }
                        _ => {
                            return Err(JitError::Parsing(
                                "Expected field name after '.'".into(),
                                self.line,
                                0,
                            ));
                        }
                    }
                }
                _ => break,
            }
        }

        self.expect(Token::Colon)?;

        // Optimization: x: x + 1
        if accessors.is_empty() {
            if let Some(Token::Identifier(rhs_id)) = self.peek()
                && rhs_id == id
                && matches!(self.peek_n(1), Some(Token::Plus))
                && matches!(self.peek_n(2), Some(Token::Number(1.0)))
            {
                let t3 = self.peek_n(3);
                if matches!(
                    t3,
                    None | Some(Token::Newline)
                        | Some(Token::RBrace)
                        | Some(Token::Comma)
                        | Some(Token::RParen)
                ) {
                    self.advance()?;
                    self.advance()?;
                    self.advance()?;
                    if info.is_global {
                        instructions.push(Instruction::IncrementGlobal(info.idx));
                    } else {
                        instructions.push(Instruction::Increment(info.idx));
                    }
                    return Ok(());
                }
            }
            if let Some(Token::Number(1.0)) = self.peek()
                && matches!(self.peek_n(1), Some(Token::Plus))
                && let Some(Token::Identifier(rhs_id)) = self.peek_n(2)
                && rhs_id == id
            {
                let t3 = self.peek_n(3);
                if matches!(
                    t3,
                    None | Some(Token::Newline)
                        | Some(Token::RBrace)
                        | Some(Token::Comma)
                        | Some(Token::RParen)
                ) {
                    self.advance()?;
                    self.advance()?;
                    self.advance()?;
                    if info.is_global {
                        instructions.push(Instruction::IncrementGlobal(info.idx));
                    } else {
                        instructions.push(Instruction::Increment(info.idx));
                    }
                    return Ok(());
                }
            }
        }

        let src = self.parse_expr(instructions)?;
        if accessors.is_empty() {
            if !info.is_mut {
                return Err(JitError::RedefinitionOfImmutableVariable(
                    id.into(),
                    self.line,
                    0,
                    info.first_line,
                ));
            }
            if info.is_global {
                instructions.push(Instruction::StoreGlobal {
                    global: info.idx,
                    src,
                });
            } else {
                instructions.push(Instruction::Move { dst: info.idx, src });
            }
        } else {
            let mut current = self.load_var(info, instructions);
            for i in 0..accessors.len() - 1 {
                let dst = self.alloc_reg();
                match accessors[i] {
                    Accessor::Index(index_reg) => instructions.push(Instruction::ListGet {
                        dst,
                        list: current,
                        index_reg,
                        loc,
                    }),
                    Accessor::Field(name_id) => instructions.push(Instruction::ObjectGet {
                        dst,
                        obj: current,
                        name_id,
                        loc,
                    }),
                }
                current = dst;
            }
            match accessors.last().unwrap() {
                Accessor::Index(index_reg) => instructions.push(Instruction::ListSet {
                    list: current,
                    index_reg: *index_reg,
                    src,
                    loc,
                }),
                Accessor::Field(name_id) => instructions.push(Instruction::ObjectSet {
                    obj: current,
                    name_id: *name_id,
                    src,
                    loc,
                }),
            }
        }
        Ok(())
    }

    fn parse_var_decl(
        &mut self,
        is_mut: bool,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        self.advance()?; // consume el/le
        let id = match self.advance()? {
            (Token::Identifier(id), _) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected identifier".into(),
                    self.line,
                    0,
                ));
            }
        };
        self.expect(Token::Colon)?;
        let src = self.parse_expr(instructions)?;

        let is_global = !self.is_in_function && !self.is_in_spawn;
        let idx = if is_global {
            let i = self.next_global;
            self.next_global += 1;
            i
        } else {
            self.alloc_reg()
        };
        let info = VarInfo {
            idx,
            is_mut,
            is_global,
            first_line: self.line,
        };

        if is_global {
            self.globals.insert(id, info);
            instructions.push(Instruction::StoreGlobal { global: idx, src });
        } else {
            self.locals.insert(id, info);
            instructions.push(Instruction::Move { dst: idx, src });
        }
        Ok(())
    }

    fn parse_block(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        self.skip_newlines();
        self.expect(Token::LBrace)?;
        while self.peek().is_some() && self.peek() != Some(Token::RBrace) {
            if let Some(res) = self.parse_statement(instructions) {
                res?;
            } else {
                break;
            }
        }
        self.expect(Token::RBrace)?;
        Ok(())
    }

    fn parse_if_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let cond = self.parse_expr(instructions)?;
        let jump_if_false_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;

        if matches!(self.peek(), Some(Token::Else)) {
            self.advance()?;
            let jump_to_end_idx = instructions.len();
            instructions.push(Instruction::Jump(0));
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };
            self.parse_block(instructions)?;
            instructions[jump_to_end_idx] = Instruction::Jump(instructions.len());
        } else {
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: instructions.len(),
            };
        }
        Ok(())
    }

    fn parse_while_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let start = instructions.len();
        let cond = self.parse_expr(instructions)?;
        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;
        instructions.push(Instruction::Jump(start));
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond,
            target: instructions.len(),
        };
        Ok(())
    }

    fn parse_for_loop(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let loc = self.loc();
        self.advance()?; // for
        let id = match self.advance()? {
            (Token::Identifier(id), _) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected identifier".into(),
                    self.line,
                    0,
                ));
            }
        };
        self.expect(Token::In)?;
        let start_val = self.parse_expr(instructions)?;
        self.expect(Token::Range)?;
        let end_val = self.parse_expr(instructions)?;

        let var_idx = self.alloc_reg();
        self.locals.insert(
            id,
            VarInfo {
                idx: var_idx,
                is_mut: true,
                is_global: false,
                first_line: self.line,
            },
        );
        instructions.push(Instruction::Move {
            dst: var_idx,
            src: start_val,
        });

        let loop_start = instructions.len();
        let cond = self.alloc_reg();
        instructions.push(Instruction::Lt {
            dst: cond,
            lhs: var_idx,
            rhs: end_val,
            loc,
        });
        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0));

        self.parse_block(instructions)?;
        instructions.push(Instruction::Increment(var_idx));
        instructions.push(Instruction::Jump(loop_start));
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond,
            target: instructions.len(),
        };
        Ok(())
    }

    fn parse_spawn_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let old_spawn = self.is_in_spawn;
        self.is_in_spawn = true;
        self.captures_stack.push(FxHashSet::default());
        self.spawn_start_regs.push(self.next_reg);

        let mut body = Vec::new();
        let regs_at_start = self.next_reg;
        self.parse_block(&mut body)?;

        let captures_set = self.captures_stack.pop().unwrap();
        self.spawn_start_regs.pop();
        self.is_in_spawn = old_spawn;

        let mut captures: Vec<usize> = captures_set.into_iter().collect();
        captures.sort_unstable();

        instructions.push(Instruction::Spawn {
            instructions: Arc::from(body),
            locals_count: self.next_reg.max(regs_at_start),
            captures: Arc::from(captures),
        });
        Ok(())
    }

    fn parse_fn_decl(&mut self) -> Result<(), JitError> {
        let name = match self.advance()? {
            (Token::Identifier(id), _) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected function name".into(),
                    self.line,
                    0,
                ));
            }
        };
        self.expect(Token::LParen)?;
        let mut params = Vec::new();
        if !matches!(self.peek(), Some(Token::RParen)) {
            loop {
                match self.advance()? {
                    (Token::Identifier(id), _) => params.push(id),
                    _ => break,
                }
                if matches!(self.peek(), Some(Token::Comma)) {
                    self.advance()?;
                } else {
                    break;
                }
            }
        }
        self.expect(Token::RParen)?;

        let old_locals = std::mem::take(&mut self.locals);
        let (old_reg, old_spawn, old_func) = (self.next_reg, self.is_in_spawn, self.is_in_function);
        self.next_reg = 0;
        self.is_in_spawn = false;
        self.is_in_function = true;

        for &p in &params {
            let r = self.alloc_reg();
            self.locals.insert(
                p,
                VarInfo {
                    idx: r,
                    is_mut: false,
                    is_global: false,
                    first_line: self.line,
                },
            );
        }

        let mut body = Vec::new();
        self.parse_block(&mut body)?;
        if !matches!(body.last(), Some(Instruction::Return(_))) {
            body.push(Instruction::Return(None));
        }

        let name_id = self.intern(name);
        self.functions.push(UserFunction {
            name_id,
            instructions: Arc::from(body),
            locals_count: self.next_reg,
            params_count: params.len(),
        });

        self.locals = old_locals;
        self.next_reg = old_reg;
        self.is_in_spawn = old_spawn;
        self.is_in_function = old_func;
        Ok(())
    }

    fn parse_return_stmt(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let val = if !matches!(
            self.peek(),
            None | Some(Token::Newline) | Some(Token::RBrace)
        ) {
            Some(self.parse_expr(instructions)?)
        } else {
            None
        };
        instructions.push(Instruction::Return(val));
        Ok(())
    }

    fn get_var(&self, id: &str) -> Option<VarInfo> {
        self.locals
            .get(id)
            .or_else(|| self.globals.get(id))
            .copied()
    }

    fn load_var(&mut self, info: VarInfo, instructions: &mut Vec<Instruction>) -> usize {
        if info.is_global {
            let r = self.alloc_reg();
            instructions.push(Instruction::LoadGlobal {
                dst: r,
                global: info.idx,
            });
            r
        } else {
            self.track_capture(info.idx);
            info.idx
        }
    }

    fn track_capture(&mut self, reg: usize) {
        for i in (0..self.spawn_start_regs.len()).rev() {
            if reg < self.spawn_start_regs[i] {
                self.captures_stack[i].insert(reg);
            } else {
                break;
            }
        }
    }
}

fn unescape_string(s: &str) -> String {
    let mut res = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => res.push('\n'),
                Some('r') => res.push('\r'),
                Some('t') => res.push('\t'),
                Some('\\') => res.push('\\'),
                Some('"') => res.push('"'),
                Some('u') => {
                    let mut hex = String::with_capacity(4);
                    for _ in 0..4 {
                        if let Some(h) = chars.next() {
                            hex.push(h);
                        }
                    }
                    if let Ok(n) = u32::from_str_radix(&hex, 16) {
                        if let Some(uc) = std::char::from_u32(n) {
                            res.push(uc);
                        }
                    }
                }
                Some(other) => {
                    res.push('\\');
                    res.push(other);
                }
                None => res.push('\\'),
            }
        } else {
            res.push(c);
        }
    }
    res
}
