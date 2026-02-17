use crate::{
    ast::{Expr, Statement},
    error::JitError,
};
use rustc_hash::FxHashMap;

#[derive(Debug, Clone)]
pub enum Op<'a> {
    Print(CompiledExpr<'a>, (usize, usize)),
    Store(usize, CompiledExpr<'a>, (usize, usize)), // index, value, (line, col)
}

#[derive(Debug, Clone)]
pub enum CompiledExpr<'a> {
    Literal(Value<'a>),
    Var(usize), // index in registers
}

#[derive(Debug, Clone, Copy)]
pub enum Value<'a> {
    Number(f64),
    String(&'a str),
    Bool(bool),
}

impl<'a> std::fmt::Display for Value<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Value::Number(n) => write!(f, "{}", n),
            Value::String(s) => write!(f, "{}", s),
            Value::Bool(b) => write!(f, "{}", b),
        }
    }
}

pub struct Program<'a> {
    pub ops: Vec<Op<'a>>,
    pub locals_count: usize,
}

pub fn compile<'a>(
    statements: impl Iterator<Item = Result<Statement<'a>, JitError>>,
) -> Result<Program<'a>, JitError> {
    let mut var_map: FxHashMap<&'a str, usize> = FxHashMap::default();
    let mut ops = Vec::new();
    let mut next_reg = 0;

    for stmt in statements {
        let stmt = stmt?;
        match stmt {
            Statement::Print(expr, loc) => {
                let compiled_expr = compile_expr(&expr, &var_map, loc)?;
                ops.push(Op::Print(compiled_expr, loc));
            }
            Statement::ImmutVar(name, expr, loc) | Statement::MutVar(name, expr, loc) => {
                let reg_idx = if let Some(&idx) = var_map.get(name) {
                    idx // Reuse existing register for variable
                } else {
                    let idx = next_reg;
                    var_map.insert(name, idx);
                    next_reg += 1;
                    idx
                };
                let compiled_expr = compile_expr(&expr, &var_map, loc)?;
                ops.push(Op::Store(reg_idx, compiled_expr, loc));
            }
        }
    }

    Ok(Program {
        ops,
        locals_count: next_reg,
    })
}

fn compile_expr<'a>(
    expr: &Expr<'a>,
    var_map: &FxHashMap<&'a str, usize>,
    loc: (usize, usize),
) -> Result<CompiledExpr<'a>, JitError> {
    match expr {
        Expr::Number(n) => Ok(CompiledExpr::Literal(Value::Number(*n))),
        Expr::String(s) => Ok(CompiledExpr::Literal(Value::String(s))),
        Expr::Bool(b) => Ok(CompiledExpr::Literal(Value::Bool(*b))),
        Expr::Var(name) => {
            if let Some(&idx) = var_map.get(name) {
                Ok(CompiledExpr::Var(idx))
            } else {
                // If variable is not found during compilation of this block,
                // it might be a forward reference or truly unknown.
                // For this simple JIT, we assume variables must be declared before use
                // OR we are in a simple linear execution where we might have missed it.
                // However, since we process linearly, if it's not in var_map, it's unknown.
                Err(JitError::UnknownVariable(name.to_string(), loc.0, loc.1))
            }
        }
    }
}
