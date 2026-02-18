use crate::{
    compiler::{Instruction, Loc, Program, Value},
    error::JitError,
    lexer::Token,
};
use logos::{Lexer, Logos};
use rustc_hash::FxHashMap;
use std::sync::Arc;

#[derive(Clone)]
struct VarInfo {
    idx: usize,
    is_mut: bool,
    is_global: bool,
    first_line: usize,
}

pub struct Parser<'source> {
    lexer: Lexer<'source, Token<'source>>,
    line: usize,
    line_start: usize,
    var_map: FxHashMap<&'source str, VarInfo>,
    /// Global string pool for interning.
    strings: Vec<Arc<str>>,
    string_map: FxHashMap<String, u32>,
    next_reg: usize,
    next_global: usize,
    is_in_spawn: bool,
    captures_stack: Vec<std::collections::HashSet<usize>>,
    spawn_start_regs: Vec<usize>,
    functions: Vec<crate::compiler::UserFunction>,
    function_map: FxHashMap<String, u32>,
}

impl<'source> Parser<'source> {
    pub fn new(input: &'source str) -> Self {
        Self {
            lexer: Token::lexer(input),
            line: 1,
            line_start: 0,
            var_map: FxHashMap::default(),
            strings: Vec::new(),
            string_map: FxHashMap::default(),
            next_reg: 0,
            next_global: 0,
            is_in_spawn: false,
            captures_stack: Vec::new(),
            spawn_start_regs: Vec::new(),
            functions: Vec::new(),
            function_map: FxHashMap::default(),
        }
    }

    pub fn compile(mut self) -> Result<Program, JitError> {
        let mut instructions = Vec::new();
        while let Some(res) = self.parse_statement(&mut instructions) {
            res?;
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
            self.strings.push(arc_s);
            self.string_map.insert(s.to_string(), id);
            id
        }
    }

    fn loc(&self) -> Loc {
        let col = self.lexer.span().start.saturating_sub(self.line_start) + 1;
        Loc {
            line: self.line as u32,
            col: col as u32,
        }
    }

    fn alloc_reg(&mut self) -> usize {
        let r = self.next_reg;
        self.next_reg += 1;
        r
    }

    fn peek(&self) -> Option<Result<Token<'source>, crate::lexer::LexingError>> {
        self.lexer.clone().next()
    }

    fn next_token(&mut self) -> Option<Result<Token<'source>, crate::lexer::LexingError>> {
        let tok = self.lexer.next();
        if let Some(Ok(Token::Newline)) = tok {
            self.line += 1;
            self.line_start = self.lexer.span().end;
        }
        tok
    }

    fn expect(&mut self) -> Result<Token<'source>, JitError> {
        let loc = self.loc();
        match self.next_token() {
            Some(Ok(t)) => Ok(t),
            Some(Err(e)) => Err(JitError::Lexing(e, loc.line as usize, loc.col as usize)),
            None => Err(JitError::Parsing(
                "Unexpected EOF".into(),
                loc.line as usize,
                loc.col as usize,
            )),
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
                Some(Ok(t)) => t,
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
            self.next_token();
            let loc = self.loc();
            let rhs = self.parse_binary(prec + 1, instructions)?;
            let dst = self.alloc_reg();
            let instr = match op {
                Token::Eq => Instruction::Eq { dst, lhs, rhs, loc },
                Token::Ne => Instruction::Ne { dst, lhs, rhs, loc },
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
        let token = self.expect()?;
        match token {
            Token::LParen => {
                let r = self.parse_expr(instructions)?;
                if !matches!(self.expect()?, Token::RParen) {
                    return Err(JitError::Parsing(
                        "Expected ')'".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
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
                let val = if let Some(sso) = Value::sso(s) {
                    sso
                } else {
                    let id = self.intern(s);
                    Value::object(id)
                };
                let r = self.alloc_reg();
                instructions.push(Instruction::LoadLiteral { dst: r, val });
                Ok(r)
            }
            Token::LBracket => self.parse_list_literal(instructions),
            Token::Identifier(id) => {
                // Check if it's a function call
                if matches!(self.peek(), Some(Ok(Token::LParen))) {
                    self.next_token(); // consume (
                    let mut args = Vec::new();
                    if !matches!(self.peek(), Some(Ok(Token::RParen))) {
                        loop {
                            args.push(self.parse_expr(instructions)?);
                            match self.expect()? {
                                Token::Comma => continue,
                                Token::RParen => break,
                                _ => {
                                    return Err(JitError::Parsing(
                                        "Expected ',' or ')'".into(),
                                        self.line,
                                        0,
                                    ));
                                }
                            }
                        }
                    } else {
                        self.next_token(); // consume )
                    }
                    let dst = self.alloc_reg();
                    if let Some(&func_id) = self.function_map.get(id) {
                        instructions.push(Instruction::Call {
                            func_id,
                            args_regs: Arc::from(args),
                            dst: Some(dst),
                        });
                    } else {
                        let name_id = self.intern(id);
                        instructions.push(Instruction::CallNative {
                            name_id,
                            args_regs: Arc::from(args),
                            dst: Some(dst),
                            loc: self.loc(),
                        });
                    }
                    return Ok(dst);
                }

                let r = if let Some(&VarInfo { idx, is_global, .. }) = self.var_map.get(id) {
                    if is_global {
                        let r = self.alloc_reg();
                        instructions.push(Instruction::LoadGlobal {
                            dst: r,
                            global: idx,
                        });
                        r
                    } else {
                        self.track_capture(idx);
                        idx
                    }
                } else {
                    return Err(JitError::UnknownVariable(
                        id.into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };

                // Handle potential indexing
                let mut current_reg = r;
                while matches!(self.peek(), Some(Ok(Token::LBracket))) {
                    self.next_token();
                    let index_reg = self.parse_expr(instructions)?;
                    if !matches!(self.next_token(), Some(Ok(Token::RBracket))) {
                        return Err(JitError::Parsing("Expected ']'".into(), self.line, 0));
                    }
                    let dst = self.alloc_reg();
                    instructions.push(Instruction::ListGet {
                        dst,
                        list: current_reg,
                        index_reg,
                        loc: self.loc(),
                    });
                    current_reg = dst;
                }
                Ok(current_reg)
            }
            _ => Err(JitError::Parsing(
                "Expected expression".into(),
                loc.line as usize,
                loc.col as usize,
            )),
        }
    }

    fn parse_list_literal(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Result<usize, JitError> {
        let mut elements = Vec::new();
        if !matches!(self.peek(), Some(Ok(Token::RBracket))) {
            loop {
                elements.push(self.parse_expr(instructions)?);
                match self.next_token() {
                    Some(Ok(Token::Comma)) => continue,
                    Some(Ok(Token::RBracket)) => break,
                    _ => {
                        return Err(JitError::Parsing(
                            "Expected ',' or ']'".into(),
                            self.line,
                            0,
                        ));
                    }
                }
            }
        } else {
            self.next_token();
        }

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

    fn parse_statement(
        &mut self,
        instructions: &mut Vec<Instruction>,
    ) -> Option<Result<(), JitError>> {
        let token = self.next_token()?;
        let loc = self.loc();
        match token {
            Ok(Token::MutableVar) => Some(self.parse_var(true, instructions)),
            Ok(Token::ImmutableVar) => Some(self.parse_var(false, instructions)),
            Ok(Token::For) => Some(self.parse_for(instructions)),
            Ok(Token::While) => Some(self.parse_while(instructions)),
            Ok(Token::Fn) => Some(self.parse_fn()),
            Ok(Token::If) => Some(self.parse_if(instructions)),
            Ok(Token::Return) => Some(self.parse_return(instructions)),
            Ok(Token::Spawn) => Some(self.parse_spawn(instructions)),
            Ok(Token::Identifier(id)) => Some(self.parse_id_statement(id, instructions)),
            Ok(Token::Newline) | Ok(Token::LineComment) => self.parse_statement(instructions),
            Ok(Token::RBrace) => None,
            _ => Some(Err(JitError::Parsing(
                "Unexpected token".into(),
                loc.line as usize,
                loc.col as usize,
            ))),
        }
    }

    fn parse_id_statement(
        &mut self,
        id: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let next = self.peek();
        match next {
            Some(Ok(Token::Colon)) | Some(Ok(Token::LBracket)) => {
                // If it's a known variable or followed by indexing/colon, treat as assignment
                self.parse_assignment(id, instructions)
            }
            _ => {
                // Otherwise treat as call (command style or function style)
                self.parse_call_statement(id, instructions)
            }
        }
    }

    fn parse_call_statement(
        &mut self,
        name: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let mut args = Vec::new();
        let loc = self.loc();

        if matches!(self.peek(), Some(Ok(Token::LParen))) {
            // Function style: name(arg1, arg2)
            self.next_token();
            if !matches!(self.peek(), Some(Ok(Token::RParen))) {
                loop {
                    args.push(self.parse_expr(instructions)?);
                    match self.next_token() {
                        Some(Ok(Token::Comma)) => continue,
                        Some(Ok(Token::RParen)) => break,
                        _ => {
                            return Err(JitError::Parsing(
                                "Expected ',' or ')'".into(),
                                self.line,
                                0,
                            ));
                        }
                    }
                }
            } else {
                self.next_token();
            }
        } else {
            // Command style: name arg1, arg2 (no parens)
            loop {
                match self.peek() {
                    Some(Ok(Token::Newline)) | Some(Ok(Token::RBrace)) | None => break,
                    _ => {
                        args.push(self.parse_expr(instructions)?);
                        if matches!(self.peek(), Some(Ok(Token::Comma))) {
                            self.next_token();
                        } else {
                            break;
                        }
                    }
                }
            }
        }

        if let Some(&func_id) = self.function_map.get(name) {
            instructions.push(Instruction::Call {
                func_id,
                args_regs: Arc::from(args),
                dst: None,
            });
        } else {
            let name_id = self.intern(name);
            instructions.push(Instruction::CallNative {
                name_id,
                args_regs: Arc::from(args),
                dst: None,
                loc,
            });
        }
        Ok(())
    }

    fn parse_assignment(
        &mut self,
        name: &'source str,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let loc = self.loc();
        let info = self
            .var_map
            .get(name)
            .ok_or_else(|| {
                JitError::UnknownVariable(name.into(), loc.line as usize, loc.col as usize)
            })?
            .clone();

        let mut current_list_reg = if info.is_global {
            let r = self.alloc_reg();
            instructions.push(Instruction::LoadGlobal {
                dst: r,
                global: info.idx,
            });
            r
        } else {
            self.track_capture(info.idx);
            info.idx
        };

        let mut indices = Vec::new();
        while matches!(self.peek(), Some(Ok(Token::LBracket))) {
            self.next_token();
            indices.push(self.parse_expr(instructions)?);
            if !matches!(self.next_token(), Some(Ok(Token::RBracket))) {
                return Err(JitError::Parsing("Expected ']'".into(), self.line, 0));
            }
        }

        if !matches!(self.next_token(), Some(Ok(Token::Colon))) {
            return Err(JitError::Parsing("Expected ':'".into(), self.line, 0));
        }

        let src_reg = self.parse_expr(instructions)?;

        if indices.is_empty() {
            if !info.is_mut {
                return Err(JitError::RedefinitionOfImmutableVariable(
                    name.into(),
                    loc.line as usize,
                    loc.col as usize,
                    info.first_line,
                ));
            }
            if info.is_global {
                instructions.push(Instruction::StoreGlobal {
                    global: info.idx,
                    src: src_reg,
                });
            } else {
                instructions.push(Instruction::Move {
                    dst: info.idx,
                    src: src_reg,
                });
            }
        } else {
            for i in 0..indices.len() - 1 {
                let next_list_reg = self.alloc_reg();
                instructions.push(Instruction::ListGet {
                    dst: next_list_reg,
                    list: current_list_reg,
                    index_reg: indices[i],
                    loc: self.loc(),
                });
                current_list_reg = next_list_reg;
            }
            instructions.push(Instruction::ListSet {
                list: current_list_reg,
                index_reg: *indices.last().unwrap(),
                src: src_reg,
                loc: self.loc(),
            });
        }

        Ok(())
    }

    fn parse_var(
        &mut self,
        is_mut: bool,
        instructions: &mut Vec<Instruction>,
    ) -> Result<(), JitError> {
        let loc = self.loc();
        let name = match self.next_token() {
            Some(Ok(Token::Identifier(id))) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected identifier".into(),
                    loc.line as usize,
                    loc.col as usize,
                ));
            }
        };
        if !matches!(self.next_token(), Some(Ok(Token::Colon))) {
            return Err(JitError::Parsing(
                "Expected ':'".into(),
                loc.line as usize,
                loc.col as usize,
            ));
        }

        let is_global = !self.is_in_spawn;
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
        self.var_map.insert(name, info.clone());

        let src_reg = self.parse_expr(instructions)?;
        if is_global {
            instructions.push(Instruction::StoreGlobal {
                global: idx,
                src: src_reg,
            });
        } else {
            instructions.push(Instruction::Move {
                dst: idx,
                src: src_reg,
            });
        }
        Ok(())
    }

    fn parse_block(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        loop {
            match self.peek() {
                Some(Ok(Token::Newline)) | Some(Ok(Token::LineComment)) => {
                    self.next_token();
                }
                _ => break,
            }
        }
        if !matches!(self.next_token(), Some(Ok(Token::LBrace))) {
            return Err(JitError::Parsing("Expected '{'".into(), self.line, 0));
        }
        while let Some(res) = self.parse_statement(instructions) {
            res?;
        }
        Ok(())
    }

    fn parse_for(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let loc = self.loc();
        let it_var = match self.next_token() {
            Some(Ok(Token::Identifier(id))) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected identifier".into(),
                    self.line,
                    0,
                ));
            }
        };
        if !matches!(self.next_token(), Some(Ok(Token::In))) {
            return Err(JitError::Parsing("Expected 'in'".into(), self.line, 0));
        }
        let start = self.parse_expr(instructions)?;
        if !matches!(self.next_token(), Some(Ok(Token::Range))) {
            return Err(JitError::Parsing("Expected '..'".into(), self.line, 0));
        }
        let end = self.parse_expr(instructions)?;
        let var_idx = self.alloc_reg();
        self.var_map.insert(
            it_var,
            VarInfo {
                idx: var_idx,
                is_mut: true,
                is_global: false,
                first_line: self.line,
            },
        );
        instructions.push(Instruction::Move {
            dst: var_idx,
            src: start,
        });
        let loop_start = instructions.len();
        let cond_reg = self.alloc_reg();
        instructions.push(Instruction::Lt {
            dst: cond_reg,
            lhs: var_idx,
            rhs: end,
            loc,
        });
        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;
        instructions.push(Instruction::Increment(var_idx));
        instructions.push(Instruction::Jump(loop_start));
        let end_pc = instructions.len();
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond: cond_reg,
            target: end_pc,
        };
        Ok(())
    }

    fn parse_while(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let start_pc = instructions.len();
        let cond = self.parse_expr(instructions)?;
        let jump_idx = instructions.len();
        instructions.push(Instruction::Jump(0));
        self.parse_block(instructions)?;
        instructions.push(Instruction::Jump(start_pc));
        let end_pc = instructions.len();
        instructions[jump_idx] = Instruction::JumpIfFalse {
            cond,
            target: end_pc,
        };
        Ok(())
    }

    fn parse_spawn(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let was_in_spawn = self.is_in_spawn;
        self.is_in_spawn = true;
        self.captures_stack.push(std::collections::HashSet::new());
        self.spawn_start_regs.push(self.next_reg);

        let mut body = Vec::new();
        let regs_at_start = self.next_reg;
        self.parse_block(&mut body)?;

        let captures_set = self.captures_stack.pop().unwrap();
        self.spawn_start_regs.pop();
        self.is_in_spawn = was_in_spawn;

        let mut captures: Vec<usize> = captures_set.into_iter().collect();
        captures.sort_unstable(); // Sort for deterministic results

        instructions.push(Instruction::Spawn {
            instructions: Arc::from(body),
            locals_count: self.next_reg.max(regs_at_start),
            captures: Arc::from(captures),
        });
        Ok(())
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

    fn parse_fn(&mut self) -> Result<(), JitError> {
        let loc = self.loc();
        let name = match self.next_token() {
            Some(Ok(Token::Identifier(id))) => id,
            _ => {
                return Err(JitError::Parsing(
                    "Expected identifier after 'fn'".into(),
                    loc.line as usize,
                    loc.col as usize,
                ));
            }
        };

        if !matches!(self.next_token(), Some(Ok(Token::LParen))) {
            return Err(JitError::Parsing("Expected '('".into(), self.line, 0));
        }

        let mut params = Vec::new();
        if !matches!(self.peek(), Some(Ok(Token::RParen))) {
            loop {
                match self.next_token() {
                    Some(Ok(Token::Identifier(id))) => params.push(id),
                    _ => {
                        return Err(JitError::Parsing(
                            "Expected parameter name".into(),
                            self.line,
                            0,
                        ));
                    }
                }
                if matches!(self.peek(), Some(Ok(Token::Comma))) {
                    self.next_token();
                } else {
                    break;
                }
            }
        }

        if !matches!(self.next_token(), Some(Ok(Token::RParen))) {
            return Err(JitError::Parsing("Expected ')'".into(), self.line, 0));
        }

        let func_id = self.functions.len() as u32;
        self.function_map.insert(name.to_string(), func_id);

        let old_vars = self.var_map.clone();
        let old_next_reg = self.next_reg;
        let old_is_in_spawn = self.is_in_spawn;

        self.var_map.clear();
        self.next_reg = 0;
        self.is_in_spawn = false;

        for &p in &params {
            let r = self.alloc_reg();
            self.var_map.insert(
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
        self.functions.push(crate::compiler::UserFunction {
            name_id,
            instructions: Arc::from(body),
            locals_count: self.next_reg,
            params_count: params.len(),
        });

        self.var_map = old_vars;
        self.next_reg = old_next_reg;
        self.is_in_spawn = old_is_in_spawn;

        Ok(())
    }

    fn parse_return(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let val = if !matches!(
            self.peek(),
            Some(Ok(Token::Newline)) | Some(Ok(Token::RBrace)) | None
        ) {
            Some(self.parse_expr(instructions)?)
        } else {
            None
        };
        instructions.push(Instruction::Return(val));
        Ok(())
    }

    fn parse_if(&mut self, instructions: &mut Vec<Instruction>) -> Result<(), JitError> {
        let cond = self.parse_expr(instructions)?;
        let jump_if_false_idx = instructions.len();
        instructions.push(Instruction::Jump(0)); // Placeholder

        self.parse_block(instructions)?;

        if matches!(self.peek(), Some(Ok(Token::Else))) {
            self.next_token(); // consume else
            let jump_to_end_idx = instructions.len();
            instructions.push(Instruction::Jump(0)); // Placeholder for skip else

            let else_start_pc = instructions.len();
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: else_start_pc,
            };

            self.parse_block(instructions)?;
            let end_pc = instructions.len();
            instructions[jump_to_end_idx] = Instruction::Jump(end_pc);
        } else {
            let end_pc = instructions.len();
            instructions[jump_if_false_idx] = Instruction::JumpIfFalse {
                cond,
                target: end_pc,
            };
        }

        Ok(())
    }
}
