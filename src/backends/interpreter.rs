use crate::{
    backends::{Backend, Context, setup_native_fns},
    compiler::{Instruction, Program, Value},
    error::JitError,
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::task::JoinSet;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>> {
        Box::pin(async move { run_interpreter(program).await })
    }
}

async fn run_interpreter(program: Program) -> Result<(), JitError> {
    // Initialize lock-free globals with 0 (which is an empty double).
    let mut globals = Vec::with_capacity(program.globals_count);
    for _ in 0..program.globals_count {
        globals.push(AtomicU64::new(0));
    }

    let mut registry = rustc_hash::FxHashMap::default();
    setup_native_fns(&mut registry);

    // Bake registry into a dense Vec for O(1) lookup during execution.
    let mut native_fns = vec![None; program.string_pool.len()];
    for (name, func) in registry {
        for (id, s) in program.string_pool.iter().enumerate() {
            if s.as_ref() == name {
                native_fns[id] = Some(func.clone());
            }
        }
    }

    let ctx = Arc::new(Context {
        globals,
        string_pool: program.string_pool.clone(),
        lists: std::sync::RwLock::new(Vec::new()),
        native_fns,
    });

    let mut registers = vec![Value::from_bits(0); program.locals_count];
    let mut join_set = JoinSet::new();

    execute_bytecode(
        program.instructions.clone(),
        ctx.clone(),
        &mut join_set,
        &mut registers,
    )
    .await?;

    // Collect all background tasks.
    while let Some(res) = join_set.join_next().await {
        if let Ok(Err(e)) = res {
            return Err(e);
        }
    }
    Ok(())
}

#[async_recursion::async_recursion]
async fn execute_bytecode(
    instructions: Arc<[Instruction]>,
    ctx: Arc<Context>,
    join_set: &mut JoinSet<Result<(), JitError>>,
    registers: &mut Vec<Value>,
) -> Result<(), JitError> {
    let mut pc = 0;

    while pc < instructions.len() {
        let instr = unsafe { instructions.get_unchecked(pc) };

        match instr {
            Instruction::LoadLiteral { dst, val } => {
                unsafe {
                    *registers.get_unchecked_mut(*dst) = *val;
                }
                pc += 1;
            }
            Instruction::Move { dst, src } => {
                let val = unsafe { *registers.get_unchecked(*src) };
                unsafe {
                    *registers.get_unchecked_mut(*dst) = val;
                }
                pc += 1;
            }
            Instruction::LoadGlobal { dst, global } => {
                let bits = unsafe { ctx.globals.get_unchecked(*global).load(Ordering::Relaxed) };
                unsafe {
                    *registers.get_unchecked_mut(*dst) = Value::from_bits(bits);
                }
                pc += 1;
            }
            Instruction::StoreGlobal { global, src } => {
                let bits = unsafe { registers.get_unchecked(*src).to_bits() };
                unsafe {
                    ctx.globals
                        .get_unchecked(*global)
                        .store(bits, Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::CallNative {
                name_id,
                args_regs,
                dst,
                loc,
            } => {
                if let Some(native_fn) = unsafe { ctx.native_fns.get_unchecked(*name_id as usize) }
                {
                    let mut args = Vec::with_capacity(args_regs.len());
                    for &reg in args_regs.iter() {
                        args.push(unsafe { *registers.get_unchecked(reg) });
                    }

                    let res = native_fn(ctx.clone(), args, *loc).await?;
                    if let Some(dst_reg) = dst {
                        unsafe {
                            *registers.get_unchecked_mut(*dst_reg) = res;
                        }
                    }
                } else {
                    let name = &ctx.string_pool[*name_id as usize];
                    return Err(JitError::Runtime(
                        format!("Unknown native function: {}", name),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Jump(target) => pc = *target,
            Instruction::JumpIfFalse { cond, target } => {
                let val = unsafe { registers.get_unchecked(*cond) };
                if let Some(false) = val.as_bool() {
                    pc = *target;
                } else {
                    pc += 1;
                }
            }
            Instruction::Add { dst, lhs, rhs, loc } => {
                let l = unsafe { registers.get_unchecked(*lhs) };
                let r = unsafe { registers.get_unchecked(*rhs) };
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        *registers.get_unchecked_mut(*dst) = Value::number(lv + rv);
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Math error: expected numbers".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Sub { dst, lhs, rhs, loc } => {
                let l = unsafe { registers.get_unchecked(*lhs) };
                let r = unsafe { registers.get_unchecked(*rhs) };
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        *registers.get_unchecked_mut(*dst) = Value::number(lv - rv);
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Math error: expected numbers".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Mul { dst, lhs, rhs, loc } => {
                let l = unsafe { registers.get_unchecked(*lhs) };
                let r = unsafe { registers.get_unchecked(*rhs) };
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        *registers.get_unchecked_mut(*dst) = Value::number(lv * rv);
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Math error: expected numbers".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Div { dst, lhs, rhs, loc } => {
                let l = unsafe { registers.get_unchecked(*lhs) };
                let r = unsafe { registers.get_unchecked(*rhs) };
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        *registers.get_unchecked_mut(*dst) = Value::number(lv / rv);
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Math error: expected numbers".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Increment(reg) => {
                if let Some(n) = unsafe { registers.get_unchecked_mut(*reg) }.as_number() {
                    unsafe {
                        *registers.get_unchecked_mut(*reg) = Value::number(n + 1.0);
                    }
                }
                pc += 1;
            }
            Instruction::LessThan { dst, lhs, rhs, loc } => {
                let l = unsafe { registers.get_unchecked(*lhs) };
                let r = unsafe { registers.get_unchecked(*rhs) };
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        *registers.get_unchecked_mut(*dst) = Value::bool(lv < rv);
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Compare error: expected numbers".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::NewList { dst, len } => {
                let list = vec![Value::from_bits(0); *len];
                let list_arc = Arc::new(std::sync::RwLock::new(list));
                let mut lists = ctx.lists.write().unwrap();
                let id = lists.len() as u32;
                lists.push(list_arc);
                unsafe {
                    *registers.get_unchecked_mut(*dst) = Value::list_id(id);
                }
                pc += 1;
            }
            Instruction::ListGet {
                dst,
                list,
                index_reg,
                loc,
            } => {
                let list_val = unsafe { *registers.get_unchecked(*list) };
                let index_val = unsafe { *registers.get_unchecked(*index_reg) };
                let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| {
                    JitError::Runtime(
                        "List index must be a number".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                if let Some(lid) = list_val.as_list_id() {
                    let lists = ctx.lists.read().unwrap();
                    if let Some(list_arc) = lists.get(lid as usize) {
                        let list = list_arc.read().unwrap();
                        if let Some(val) = list.get(index) {
                            unsafe {
                                *registers.get_unchecked_mut(*dst) = *val;
                            }
                        } else {
                            return Err(JitError::Runtime(
                                format!(
                                    "Index out of bounds: {} for list of length {}",
                                    index,
                                    list.len()
                                ),
                                loc.line as usize,
                                loc.col as usize,
                            ));
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Invalid list ID".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Expected list for indexing".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::ListSet {
                list,
                index_reg,
                src,
                loc,
            } => {
                let list_val = unsafe { *registers.get_unchecked(*list) };
                let index_val = unsafe { *registers.get_unchecked(*index_reg) };
                let val = unsafe { *registers.get_unchecked(*src) };
                let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| {
                    JitError::Runtime(
                        "List index must be a number".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                if let Some(lid) = list_val.as_list_id() {
                    let lists = ctx.lists.read().unwrap();
                    if let Some(list_arc) = lists.get(lid as usize) {
                        let mut list = list_arc.write().unwrap();
                        if let Some(slot) = list.get_mut(index) {
                            *slot = val;
                        } else {
                            return Err(JitError::Runtime(
                                format!(
                                    "Index out of bounds: {} for list of length {}",
                                    index,
                                    list.len()
                                ),
                                loc.line as usize,
                                loc.col as usize,
                            ));
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Invalid list ID".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                } else {
                    return Err(JitError::Runtime(
                        "Expected list for indexing".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::Spawn {
                instructions: body,
                locals_count,
            } => {
                let body = Arc::clone(body);
                let count = *locals_count;
                let s_ctx = ctx.clone();
                let mut thread_regs = registers.clone();
                if thread_regs.len() < count {
                    thread_regs.resize(count, Value::from_bits(0));
                }

                join_set.spawn(async move {
                    let mut js = JoinSet::new();
                    execute_bytecode(body, s_ctx, &mut js, &mut thread_regs).await?;
                    while let Some(res) = js.join_next().await {
                        if let Ok(Err(e)) = res {
                            return Err(e);
                        }
                    }
                    Ok(())
                });
                pc += 1;
            }
        }
        if pc & 0x1FF == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok(())
}
