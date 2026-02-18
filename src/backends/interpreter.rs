use crate::{
    backends::{Backend, Context, Generation, ManagedObject, setup_native_fns},
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
    let mut globals = Vec::with_capacity(program.globals_count);
    for _ in 0..program.globals_count {
        globals.push(AtomicU64::new(0));
    }

    let mut registry = rustc_hash::FxHashMap::default();
    setup_native_fns(&mut registry);

    // Initial heap with literal strings from the pool.
    let mut heap_init = Vec::with_capacity(program.string_pool.len());
    for s in program.string_pool.iter() {
        heap_init.push(Some(crate::backends::HeapObject {
            obj: ManagedObject::String(s.clone()),
            last_gc_id: 0,
            generation: Generation::Tenured,
        }));
    }

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
        heap: std::sync::RwLock::new(heap_init),
        free_list: std::sync::Mutex::new(Vec::new()),
        nursery_ids: std::sync::Mutex::new(Vec::new()),
        native_fns,
        active_registers: std::sync::RwLock::new(Vec::new()),
        remembered_set: std::sync::Mutex::new(rustc_hash::FxHashSet::default()),
        gc_count: std::sync::atomic::AtomicU32::new(0),
        alloc_since_gc: std::sync::atomic::AtomicUsize::new(0),
        functions: program.functions.clone(),
    });

    // Initialize initial registers
    let mut regs = Vec::with_capacity(program.locals_count);
    for _ in 0..program.locals_count {
        regs.push(AtomicU64::new(0));
    }
    let registers: Arc<[AtomicU64]> = Arc::from(regs);

    let mut join_set = JoinSet::new();

    execute_bytecode(
        program.instructions.clone(),
        ctx.clone(),
        &mut join_set,
        registers,
        None,
    )
    .await?;

    while let Some(res) = join_set.join_next().await {
        if let Ok(Err(e)) = res {
            return Err(e);
        }
    }
    Ok(())
}

#[async_recursion::async_recursion]
pub async fn execute_bytecode(
    instructions: Arc<[Instruction]>,
    ctx: Arc<Context>,
    join_set: &mut JoinSet<Result<(), JitError>>,
    registers: Arc<[AtomicU64]>,
    dst_reg: Option<&AtomicU64>,
) -> Result<Value, JitError> {
    // Register for GC
    {
        let mut active = ctx.active_registers.write().unwrap();
        active.push(registers.clone());
    }

    // De-register on drop
    struct RegGuard {
        ctx: Arc<Context>,
        regs: Arc<[AtomicU64]>,
    }
    impl Drop for RegGuard {
        fn drop(&mut self) {
            let mut active = self.ctx.active_registers.write().unwrap();
            active.retain(|r| !Arc::ptr_eq(r, &self.regs));
        }
    }
    let _guard = RegGuard {
        ctx: ctx.clone(),
        regs: registers.clone(),
    };

    let mut pc = 0;
    let mut instr_count: u32 = 0;
    while pc < instructions.len() {
        let instr = unsafe { instructions.get_unchecked(pc) };
        instr_count = instr_count.wrapping_add(1);

        match instr {
            Instruction::LoadLiteral { dst, val } => {
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(val.to_bits(), Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::Move { dst, src } => {
                let bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };
                unsafe {
                    registers.get_unchecked(*dst).store(bits, Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::LoadGlobal { dst, global } => {
                let bits = unsafe { ctx.globals.get_unchecked(*global).load(Ordering::Relaxed) };
                unsafe {
                    registers.get_unchecked(*dst).store(bits, Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::StoreGlobal { global, src } => {
                let bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };
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
                let native_fn = ctx
                    .native_fns
                    .get(*name_id as usize)
                    .and_then(|f| f.clone());
                if let Some(native_fn) = native_fn {
                    let mut args = Vec::with_capacity(args_regs.len());
                    for &reg in args_regs.iter() {
                        args.push(Value::from_bits(unsafe {
                            registers.get_unchecked(reg).load(Ordering::Relaxed)
                        }));
                    }

                    let res = native_fn(ctx.clone(), args, *loc).await?;
                    if let Some(dst_reg) = dst {
                        unsafe {
                            registers
                                .get_unchecked(*dst_reg)
                                .store(res.to_bits(), Ordering::Relaxed);
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
            Instruction::Call {
                func_id,
                args_regs,
                dst,
                loc,
            } => {
                let func = ctx.functions.get(*func_id as usize).cloned().unwrap();
                if args_regs.len() != func.params_count {
                    return Err(JitError::Runtime(
                        format!(
                            "Function call arity mismatch: expected {}, got {}",
                            func.params_count,
                            args_regs.len()
                        ),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                let mut f_regs_vec = Vec::with_capacity(func.locals_count);
                for _ in 0..func.locals_count {
                    f_regs_vec.push(AtomicU64::new(0));
                }
                for (i, &reg) in args_regs.iter().enumerate() {
                    let bits = unsafe { registers.get_unchecked(reg).load(Ordering::Relaxed) };
                    f_regs_vec[i].store(bits, Ordering::Relaxed);
                }
                let f_regs: Arc<[AtomicU64]> = Arc::from(f_regs_vec);

                let _ = execute_bytecode(
                    func.instructions.clone(),
                    ctx.clone(),
                    join_set,
                    f_regs,
                    dst.map(|r| unsafe { registers.get_unchecked(r) }),
                )
                .await?;

                pc += 1;
            }
            Instruction::Return(val_reg) => {
                let val = if let Some(reg) = val_reg {
                    Value::from_bits(unsafe {
                        registers.get_unchecked(*reg).load(Ordering::Relaxed)
                    })
                } else {
                    Value::from_bits(0)
                };
                if let Some(dst) = dst_reg {
                    dst.store(val.to_bits(), Ordering::Relaxed);
                }
                return Ok(val);
            }
            Instruction::Jump(target) => pc = *target,
            Instruction::JumpIfFalse { cond, target } => {
                let val = Value::from_bits(unsafe {
                    registers.get_unchecked(*cond).load(Ordering::Relaxed)
                });
                if let Some(false) = val.as_bool() {
                    pc = *target;
                } else {
                    pc += 1;
                }
            }
            Instruction::Add { dst, lhs, rhs, loc } => {
                const QNAN: u64 = 0x7ff0000000000000;
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };

                if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    let res = f64::from_bits(l_bits) + f64::from_bits(r_bits);
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::number(res).to_bits(), Ordering::Relaxed);
                    }
                } else {
                    let l = Value::from_bits(l_bits);
                    let r = Value::from_bits(r_bits);
                    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                        unsafe {
                            registers
                                .get_unchecked(*dst)
                                .store(Value::number(lv + rv).to_bits(), Ordering::Relaxed);
                        }
                    } else if let (Some(ls), Some(rs)) =
                        (ctx.get_string_value(l), ctx.get_string_value(r))
                    {
                        let combined = ls + &rs;
                        if let Some(sso) = Value::sso(&combined) {
                            unsafe {
                                registers
                                    .get_unchecked(*dst)
                                    .store(sso.to_bits(), Ordering::Relaxed);
                            }
                        } else {
                            ctx.alloc(ManagedObject::String(Arc::from(combined)), unsafe {
                                registers.get_unchecked(*dst)
                            });
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Add error: expected numbers or strings".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                }
                pc += 1;
            }
            Instruction::Sub { dst, lhs, rhs, loc } => {
                let l = Value::from_bits(registers[*lhs].load(Ordering::Relaxed));
                let r = Value::from_bits(registers[*rhs].load(Ordering::Relaxed));
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    registers[*dst].store(Value::number(lv - rv).to_bits(), Ordering::Relaxed);
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
                let l = Value::from_bits(registers[*lhs].load(Ordering::Relaxed));
                let r = Value::from_bits(registers[*rhs].load(Ordering::Relaxed));
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    registers[*dst].store(Value::number(lv * rv).to_bits(), Ordering::Relaxed);
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
                let l = Value::from_bits(registers[*lhs].load(Ordering::Relaxed));
                let r = Value::from_bits(registers[*rhs].load(Ordering::Relaxed));
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    registers[*dst].store(Value::number(lv / rv).to_bits(), Ordering::Relaxed);
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
                let _ = unsafe {
                    registers.get_unchecked(*reg).fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |bits| {
                            let val = Value::from_bits(bits);
                            val.as_number().map(|n| Value::number(n + 1.0).to_bits())
                        },
                    )
                };
                pc += 1;
            }
            Instruction::IncrementGlobal(global) => {
                let _ = unsafe {
                    ctx.globals.get_unchecked(*global).fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |bits| {
                            let val = Value::from_bits(bits);
                            val.as_number().map(|n| Value::number(n + 1.0).to_bits())
                        },
                    )
                };
                pc += 1;
            }
            Instruction::Eq {
                dst,
                lhs,
                rhs,
                loc: _,
            } => {
                let l = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let lv = Value::from_bits(l);
                let rv = Value::from_bits(r);
                let eq = if l == r {
                    true
                } else if let (Some(ln), Some(rn)) = (lv.as_number(), rv.as_number()) {
                    ln == rn
                } else if let (Some(ls), Some(rs)) =
                    (ctx.get_string_value(lv), ctx.get_string_value(rv))
                {
                    ls == rs
                } else {
                    false
                };
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::Ne {
                dst,
                lhs,
                rhs,
                loc: _,
            } => {
                let l = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let lv = Value::from_bits(l);
                let rv = Value::from_bits(r);
                let eq = if l == r {
                    true
                } else if let (Some(ln), Some(rn)) = (lv.as_number(), rv.as_number()) {
                    ln == rn
                } else if let (Some(ls), Some(rs)) =
                    (ctx.get_string_value(lv), ctx.get_string_value(rv))
                {
                    ls == rs
                } else {
                    false
                };
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(Value::bool(!eq).to_bits(), Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::Lt { dst, lhs, rhs, loc } => {
                let l = Value::from_bits(unsafe {
                    registers.get_unchecked(*lhs).load(Ordering::Relaxed)
                });
                let r = Value::from_bits(unsafe {
                    registers.get_unchecked(*rhs).load(Ordering::Relaxed)
                });
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(lv < rv).to_bits(), Ordering::Relaxed);
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
            Instruction::Le { dst, lhs, rhs, loc } => {
                let l = Value::from_bits(unsafe {
                    registers.get_unchecked(*lhs).load(Ordering::Relaxed)
                });
                let r = Value::from_bits(unsafe {
                    registers.get_unchecked(*rhs).load(Ordering::Relaxed)
                });
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(lv <= rv).to_bits(), Ordering::Relaxed);
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
            Instruction::Gt { dst, lhs, rhs, loc } => {
                let l = Value::from_bits(unsafe {
                    registers.get_unchecked(*lhs).load(Ordering::Relaxed)
                });
                let r = Value::from_bits(unsafe {
                    registers.get_unchecked(*rhs).load(Ordering::Relaxed)
                });
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(lv > rv).to_bits(), Ordering::Relaxed);
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
            Instruction::Ge { dst, lhs, rhs, loc } => {
                let l = Value::from_bits(unsafe {
                    registers.get_unchecked(*lhs).load(Ordering::Relaxed)
                });
                let r = Value::from_bits(unsafe {
                    registers.get_unchecked(*rhs).load(Ordering::Relaxed)
                });
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(lv >= rv).to_bits(), Ordering::Relaxed);
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
                let mut elements = Vec::with_capacity(*len);
                for _ in 0..*len {
                    elements.push(AtomicU64::new(0));
                }
                ctx.alloc(ManagedObject::List(elements.into_boxed_slice()), unsafe {
                    registers.get_unchecked(*dst)
                });
                pc += 1;
            }
            Instruction::ListGet {
                dst,
                list,
                index_reg,
                loc,
            } => {
                let list_val = Value::from_bits(unsafe {
                    registers.get_unchecked(*list).load(Ordering::Relaxed)
                });
                let index_val = Value::from_bits(unsafe {
                    registers.get_unchecked(*index_reg).load(Ordering::Relaxed)
                });
                let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| {
                    JitError::Runtime(
                        "List index must be a number".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                if let Some(oid) = list_val.as_obj_id() {
                    let heap = ctx.heap.read().unwrap();
                    if let Some(Some(crate::backends::HeapObject {
                        obj: ManagedObject::List(elements),
                        ..
                    })) = heap.get(oid as usize)
                    {
                        if let Some(atomic_val) = elements.get(index) {
                            let val_bits = atomic_val.load(Ordering::Relaxed);
                            unsafe {
                                registers
                                    .get_unchecked(*dst)
                                    .store(val_bits, Ordering::Relaxed);
                            }
                        } else {
                            return Err(JitError::Runtime(
                                format!(
                                    "Index out of bounds: {} for list of length {}",
                                    index,
                                    elements.len()
                                ),
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
                let list_val = Value::from_bits(unsafe {
                    registers.get_unchecked(*list).load(Ordering::Relaxed)
                });
                let index_val = Value::from_bits(unsafe {
                    registers.get_unchecked(*index_reg).load(Ordering::Relaxed)
                });
                let src_bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };
                let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| {
                    JitError::Runtime(
                        "List index must be a number".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                if let Some(oid) = list_val.as_obj_id() {
                    let heap = ctx.heap.read().unwrap();
                    if let Some(Some(obj)) = heap.get(oid as usize) {
                        if let ManagedObject::List(elements) = &obj.obj {
                            if let Some(slot) = elements.get(index) {
                                slot.store(src_bits, Ordering::Relaxed);

                                // Write Barrier
                                if obj.generation == Generation::Tenured {
                                    let src_val = Value::from_bits(src_bits);
                                    if let Some(src_oid) = src_val.as_obj_id() {
                                        let src_obj_opt = heap.get(src_oid as usize);
                                        if let Some(Some(src_obj)) = src_obj_opt
                                            && src_obj.generation == Generation::Nursery
                                        {
                                            ctx.remembered_set.lock().unwrap().insert(oid);
                                        }
                                    }
                                }
                            } else {
                                return Err(JitError::Runtime(
                                    format!(
                                        "Index out of bounds: {} for list of length {}",
                                        index,
                                        elements.len()
                                    ),
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
                    } else {
                        return Err(JitError::Runtime(
                            "Expected list for indexing".into(),
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
                captures,
            } => {
                let body = Arc::clone(body);
                let count = *locals_count;
                let s_ctx = ctx.clone();

                let mut t_regs = Vec::with_capacity(count);
                for _ in 0..count {
                    t_regs.push(AtomicU64::new(0));
                }
                for &reg in captures.iter() {
                    let bits = registers[reg].load(Ordering::Relaxed);
                    t_regs[reg].store(bits, Ordering::Relaxed);
                }
                let thread_regs: Arc<[AtomicU64]> = Arc::from(t_regs);

                join_set.spawn(async move {
                    let mut js = JoinSet::new();
                    let _ = execute_bytecode(body, s_ctx, &mut js, thread_regs, None).await?;
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
        if instr_count & 0x3FF == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok(Value::from_bits(0))
}
