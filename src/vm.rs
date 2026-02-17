use crate::{
    compiler::{Op, Program, Value},
    error::JitError,
};
use std::io::{BufWriter, Write};

pub fn run(program: Program) -> Result<(), JitError> {
    let stdout = std::io::stdout();
    let mut writer = BufWriter::new(stdout.lock());
    let mut registers: Vec<Value> = Vec::with_capacity(program.locals_count);

    // Initialize registers with dummy values or Option?
    // Value is not Clone/Default easily without allocation if String is involved?
    // But Value holds &'a str.
    // Let's us `Option<Value>` for safety, or just push.
    // The compiler assigns indices. If we encounter a Store(idx, ...), we need to ensure registers[idx] is valid.
    // But `locals_count` is the max index + 1.
    // We can pre-fill with a dummy value if needed, but since we are linear, we might need random access?
    // Actually, `compile` assigns incremental indices.
    // But wait, `var_map` reuses indices? No.
    // `compile` says: `let idx = next_reg; var_map.insert(name, idx); next_reg += 1;`
    // So indices are 0 to N-1.
    // But then: `Statement::MutVar` logic: `if let Some(&idx) = var_map.get(name) { idx } else { ... }`
    // This allows reusing the register for the *same name*.
    // So indices are compact 0..N.

    // We can resize registers.
    registers.resize(program.locals_count, Value::Bool(false));

    for op in program.ops {
        match op {
            Op::Print(expr, (line, col)) => {
                let val = eval_expr(&expr, &registers, (line, col))?;
                match val {
                    Value::Number(n) => write!(writer, "{}", n),
                    Value::String(s) => write!(writer, "{}", s),
                    Value::Bool(b) => write!(writer, "{}", b),
                }
                .map_err(|e| JitError::Runtime(format!("IO Error: {}", e), line, col))?;
                writeln!(writer)
                    .map_err(|e| JitError::Runtime(format!("IO Error: {}", e), line, col))?;
            }
            Op::Store(idx, expr, (line, col)) => {
                let val = eval_expr(&expr, &registers, (line, col))?;
                if let Some(reg) = registers.get_mut(idx) {
                    *reg = val;
                } else {
                    return Err(JitError::Runtime(
                        format!("Register index out of bounds: {}", idx),
                        line,
                        col,
                    ));
                }
            }
        }
    }

    writer
        .flush()
        .map_err(|e| JitError::Runtime(format!("IO Error: {}", e), 0, 0))?;
    Ok(())
}

#[inline(always)]
fn eval_expr<'a>(
    expr: &crate::compiler::CompiledExpr<'a>,
    registers: &[Value<'a>],
    loc: (usize, usize),
) -> Result<Value<'a>, JitError> {
    match expr {
        crate::compiler::CompiledExpr::Literal(val) => Ok(*val),
        crate::compiler::CompiledExpr::Var(idx) => match registers.get(*idx) {
            Some(val) => Ok(*val),
            None => Err(JitError::Runtime(
                format!("Register index out of bounds: {}", idx),
                loc.0,
                loc.1,
            )),
        },
    }
}
