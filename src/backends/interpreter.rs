use crate::{
    backends::{
        Backend, Callable, Context, Generation, Heap, HeapMetadata, ManagedObject, NativeFn,
    },
    compiler::{Instruction, Program, QNAN, Value},
    error::JitError,
};
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>> {
        Box::pin(async move { run_interpreter(program).await })
    }
}

async fn run_interpreter(program: Program) -> Result<(), JitError> {
    let mut globals_vec = Vec::with_capacity(program.globals_count);
    for _ in 0..program.globals_count {
        globals_vec.push(AtomicU64::new(0));
    }
    let globals: Arc<[AtomicU64]> = Arc::from(globals_vec);

    let mut registry: rustc_hash::FxHashMap<String, NativeFn> = rustc_hash::FxHashMap::default();
    setup_native_fns(&mut registry);

    // Build a mutable string pool so we can add native names that aren't in source.
    let mut string_pool_vec: Vec<Arc<str>> = program.string_pool.iter().cloned().collect();
    let mut string_pool_index: rustc_hash::FxHashMap<String, u32> = string_pool_vec
        .iter()
        .enumerate()
        .map(|(i, s)| (s.to_string(), i as u32))
        .collect();

    // Ensure every native function name has a string pool entry.
    for name in registry.keys() {
        if !string_pool_index.contains_key(name.as_str()) {
            let id = string_pool_vec.len() as u32;
            let arc: Arc<str> = Arc::from(name.as_str());
            string_pool_vec.push(arc);
            string_pool_index.insert(name.clone(), id);
        }
    }

    // Initial heap objects: one entry per string pool entry (strings are tenured from birth).
    let mut heap_init = Vec::with_capacity(string_pool_vec.len());
    for s in &string_pool_vec {
        heap_init.push(Some(crate::backends::HeapObject {
            obj: ManagedObject::String(s.clone()),
            last_gc_id: 0,
            generation: Generation::Tenured,
        }));
    }

    // Unified callables array: indexed by string pool ID.
    let mut callables_vec: Vec<Option<Callable>> = vec![None; string_pool_vec.len()];

    // Add user functions
    for func in program.functions.iter() {
        if (func.name_id as usize) < callables_vec.len() {
            callables_vec[func.name_id as usize] = Some(Callable::User(func.clone()));
        }
    }

    // Add native functions
    for (name, func) in registry {
        if let Some(&id) = string_pool_index.get(name.as_str()) {
            callables_vec[id as usize] = Some(Callable::Native(func));
        }
    }

    let string_pool: Arc<[Arc<str>]> = Arc::from(string_pool_vec);
    let callables: Arc<[Option<Callable>]> = Arc::from(callables_vec);

    let ctx = Arc::new(Context {
        globals,
        string_pool,
        callables,
        active_registers: std::sync::Mutex::new(Vec::new()),
        heap: Heap {
            objects: std::sync::RwLock::new(heap_init),
            metadata: std::sync::Mutex::new(HeapMetadata {
                free_list: Vec::new(),
                nursery_ids: Vec::new(),
                remembered_set: rustc_hash::FxHashSet::default(),
            }),
            gc_count: std::sync::atomic::AtomicU32::new(0),
            alloc_since_gc: std::sync::atomic::AtomicUsize::new(0),
        },
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
        let mut active = ctx.active_registers.lock().unwrap();
        active.push(registers.clone());
    }

    // De-register on drop
    struct RegGuard {
        ctx: Arc<Context>,
        regs: Arc<[AtomicU64]>,
    }
    impl Drop for RegGuard {
        fn drop(&mut self) {
            let mut active = self.ctx.active_registers.lock().unwrap();
            if let Some(pos) = active.iter().position(|r| Arc::ptr_eq(r, &self.regs)) {
                active.swap_remove(pos);
            }
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
            Instruction::Call {
                name_id,
                args_regs,
                dst,
                loc,
            } => {
                if let Some(callable) = ctx.get_callable(*name_id) {
                    match callable {
                        Callable::Native(native_fn) => {
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
                        }
                        Callable::User(func) => {
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
                            f_regs_vec.resize_with(func.locals_count, || AtomicU64::new(0));
                            for (i, &reg) in args_regs.iter().enumerate() {
                                let bits =
                                    unsafe { registers.get_unchecked(reg).load(Ordering::Relaxed) };
                                unsafe {
                                    f_regs_vec.get_unchecked(i).store(bits, Ordering::Relaxed)
                                };
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
                        }
                    }
                } else {
                    let name = unsafe { ctx.string_pool.get_unchecked(*name_id as usize) };
                    return Err(JitError::Runtime(
                        format!("Unknown function: {}", name),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                pc += 1;
            }
            Instruction::CallDynamic {
                callee_reg,
                args_regs,
                dst,
                loc,
            } => {
                // Resolve the value in callee_reg to a string pool ID — no allocation.
                let callee_bits =
                    unsafe { registers.get_unchecked(*callee_reg).load(Ordering::Relaxed) };
                let callee_val = Value::from_bits(callee_bits);
                let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| {
                    JitError::Runtime(
                        "CallDynamic: callee is not a known function name".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                // Collect args
                let mut args_vals = Vec::with_capacity(args_regs.len());
                for &reg in args_regs.iter() {
                    args_vals.push(Value::from_bits(unsafe {
                        registers.get_unchecked(reg).load(Ordering::Relaxed)
                    }));
                }

                if let Some(callable) = ctx.get_callable(name_id) {
                    match callable {
                        Callable::Native(native_fn) => {
                            let res = native_fn(ctx.clone(), args_vals, *loc).await?;
                            if let Some(dst_reg) = dst {
                                unsafe {
                                    registers
                                        .get_unchecked(*dst_reg)
                                        .store(res.to_bits(), Ordering::Relaxed);
                                }
                            }
                        }
                        Callable::User(func) => {
                            let mut f_regs_vec = Vec::with_capacity(func.locals_count);
                            f_regs_vec.resize_with(func.locals_count, || AtomicU64::new(0));
                            for (i, val) in args_vals.iter().enumerate() {
                                if i < f_regs_vec.len() {
                                    unsafe {
                                        f_regs_vec
                                            .get_unchecked(i)
                                            .store(val.to_bits(), Ordering::Relaxed);
                                    }
                                }
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
                        }
                    }
                } else {
                    return Err(JitError::Runtime(
                        format!(
                            "CallDynamic: unknown function '{}'",
                            ctx.string_pool
                                .get(name_id as usize)
                                .map(|s| s.as_ref())
                                .unwrap_or("?")
                        ),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }

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
                    } else {
                        // Optimization: handle strings with zero allocation if they fit in SSO
                        let res_string = l
                            .with_str(&ctx, |l_str| {
                                r.with_str(&ctx, |r_str| {
                                    let mut combined =
                                        String::with_capacity(l_str.len() + r_str.len());
                                    combined.push_str(l_str);
                                    combined.push_str(r_str);
                                    combined
                                })
                            })
                            .flatten();

                        if let Some(combined) = res_string {
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
                }
                pc += 1;
            }
            Instruction::Sub { dst, lhs, rhs, loc } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };

                if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    let res = f64::from_bits(l_bits) - f64::from_bits(r_bits);
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
                                .store(Value::number(lv - rv).to_bits(), Ordering::Relaxed);
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Math error: expected numbers".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                }
                pc += 1;
            }
            Instruction::Mul { dst, lhs, rhs, loc } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };

                if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    let res = f64::from_bits(l_bits) * f64::from_bits(r_bits);
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
                                .store(Value::number(lv * rv).to_bits(), Ordering::Relaxed);
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Math error: expected numbers".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                }
                pc += 1;
            }
            Instruction::Div { dst, lhs, rhs, loc } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };

                if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    let res = f64::from_bits(l_bits) / f64::from_bits(r_bits);
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
                                .store(Value::number(lv / rv).to_bits(), Ordering::Relaxed);
                        }
                    } else {
                        return Err(JitError::Runtime(
                            "Math error: expected numbers".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                }
                pc += 1;
            }
            Instruction::Increment(reg) => {
                let reg_ptr = unsafe { registers.get_unchecked(*reg) };
                let bits = reg_ptr.load(Ordering::Relaxed);
                if (bits & QNAN) != QNAN {
                    let n = f64::from_bits(bits);
                    reg_ptr.store(Value::number(n + 1.0).to_bits(), Ordering::Relaxed);
                } else {
                    let val = Value::from_bits(bits);
                    if let Some(n) = val.as_number() {
                        reg_ptr.store(Value::number(n + 1.0).to_bits(), Ordering::Relaxed);
                    }
                }
                pc += 1;
            }
            Instruction::IncrementGlobal(global) => {
                let global_ptr = unsafe { ctx.globals.get_unchecked(*global) };
                let bits = global_ptr.load(Ordering::Relaxed);
                if (bits & QNAN) != QNAN {
                    let n = f64::from_bits(bits);
                    global_ptr.store(Value::number(n + 1.0).to_bits(), Ordering::Relaxed);
                } else {
                    let val = Value::from_bits(bits);
                    if let Some(n) = val.as_number() {
                        global_ptr.store(Value::number(n + 1.0).to_bits(), Ordering::Relaxed);
                    }
                }
                pc += 1;
            }
            Instruction::Eq { dst, lhs, rhs } => {
                let l = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let eq = if l == r && (l & QNAN) != QNAN {
                    true
                } else {
                    ctx.values_equal(Value::from_bits(l), Value::from_bits(r))
                };
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let l = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let eq = if l == r && (l & QNAN) != QNAN {
                    true
                } else {
                    ctx.values_equal(Value::from_bits(l), Value::from_bits(r))
                };
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(Value::bool(!eq).to_bits(), Ordering::Relaxed);
                }
                pc += 1;
            }
            Instruction::Lt { dst, lhs, rhs, loc } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let res = if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    Some(f64::from_bits(l_bits) < f64::from_bits(r_bits))
                } else {
                    let l = Value::from_bits(l_bits);
                    let r = Value::from_bits(r_bits);
                    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                        Some(lv < rv)
                    } else {
                        None
                    }
                };
                if let Some(eq) = res {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
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
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let res = if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    Some(f64::from_bits(l_bits) <= f64::from_bits(r_bits))
                } else {
                    let l = Value::from_bits(l_bits);
                    let r = Value::from_bits(r_bits);
                    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                        Some(lv <= rv)
                    } else {
                        None
                    }
                };
                if let Some(eq) = res {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
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
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let res = if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    Some(f64::from_bits(l_bits) > f64::from_bits(r_bits))
                } else {
                    let l = Value::from_bits(l_bits);
                    let r = Value::from_bits(r_bits);
                    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                        Some(lv > rv)
                    } else {
                        None
                    }
                };
                if let Some(eq) = res {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
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
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                let res = if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    Some(f64::from_bits(l_bits) >= f64::from_bits(r_bits))
                } else {
                    let l = Value::from_bits(l_bits);
                    let r = Value::from_bits(r_bits);
                    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                        Some(lv >= rv)
                    } else {
                        None
                    }
                };
                if let Some(eq) = res {
                    unsafe {
                        registers
                            .get_unchecked(*dst)
                            .store(Value::bool(eq).to_bits(), Ordering::Relaxed);
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
                    let heap = ctx.heap.objects.read().unwrap();
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
                let index_bits = unsafe { registers.get_unchecked(*index_reg).load(Ordering::Relaxed) };
                let src_bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };
                
                let index = if (index_bits & QNAN) != QNAN {
                    f64::from_bits(index_bits) as usize
                } else {
                    let index_val = Value::from_bits(index_bits);
                    index_val.as_number().map(|n| n as usize).ok_or_else(|| {
                        JitError::Runtime(
                            "List index must be a number".into(),
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?
                };

                if let Some(oid) = list_val.as_obj_id() {
                    let heap = ctx.heap.objects.read().unwrap();
                    if let Some(Some(obj)) = heap.get(oid as usize) {
                        if let ManagedObject::List(elements) = &obj.obj {
                            if let Some(slot) = elements.get(index) {
                                slot.store(src_bits, Ordering::Relaxed);

                                // Write Barrier
                                if obj.generation == Generation::Tenured {
                                    if (src_bits & QNAN) == QNAN {
                                        let src_val = Value::from_bits(src_bits);
                                        if let Some(src_oid) = src_val.as_obj_id() {
                                            if let Some(Some(src_obj)) = heap.get(src_oid as usize)
                                                && src_obj.generation == Generation::Nursery
                                            {
                                                ctx.heap
                                                    .metadata
                                                    .lock()
                                                    .unwrap()
                                                    .remembered_set
                                                    .insert(oid);
                                            }
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
                    let bits = unsafe { registers.get_unchecked(reg).load(Ordering::Relaxed) };
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

pub fn setup_native_fns(fns: &mut rustc_hash::FxHashMap<String, NativeFn>) {
    fns.insert(
        "print".to_string(),
        Arc::new(|ctx, args, _| {
            Box::pin(async move {
                for (i, val) in args.iter().enumerate() {
                    if i > 0 {
                        print!(" ");
                    }
                    print_value(&ctx, *val);
                }
                println!();
                let _ = std::io::stdout().flush();
                Ok(Value::from_bits(0))
            })
        }),
    );

    fns.insert(
        "len".to_string(),
        Arc::new(|ctx, args, loc| {
            Box::pin(async move {
                if args.len() != 1 {
                    return Err(JitError::Runtime(
                        "len() expects 1 argument".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                let val = args[0];
                if let Some(oid) = val.as_obj_id() {
                    let heap = ctx.heap.objects.read().unwrap();
                    if let Some(Some(obj)) = heap.get(oid as usize) {
                        match &obj.obj {
                            ManagedObject::String(s) => return Ok(Value::number(s.len() as f64)),
                            ManagedObject::List(l) => return Ok(Value::number(l.len() as f64)),
                        }
                    }
                } else if let Some(s) = val.as_string(&ctx) {
                    return Ok(Value::number(s.len() as f64));
                }
                Err(JitError::Runtime(
                    "len() expects string or list".into(),
                    loc.line as usize,
                    loc.col as usize,
                ))
            })
        }),
    );

    fns.insert(
        "time".to_string(),
        Arc::new(|_, _, _| {
            Box::pin(async move {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs_f64();
                Ok(Value::number(now))
            })
        }),
    );

    fns.insert(
        "sleep".to_string(),
        Arc::new(|_, args, loc| {
            Box::pin(async move {
                if args.len() != 1 {
                    return Err(JitError::Runtime(
                        "sleep() expects 1 argument".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                if let Some(ms) = args[0].as_number() {
                    tokio::time::sleep(tokio::time::Duration::from_millis(ms as u64)).await;
                    Ok(Value::from_bits(0))
                } else {
                    Err(JitError::Runtime(
                        "sleep() expects numeric milliseconds".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            })
        }),
    );

    fns.insert(
        "fetch".to_string(),
        Arc::new(|ctx, args, loc| {
            Box::pin(async move {
                if args.is_empty() {
                    return Err(JitError::Runtime(
                        "fetch requires at least 1 argument".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                let val = args[0];
                let url = val.as_string(&ctx).ok_or_else(|| {
                    JitError::Runtime(
                        "fetch error: expected string URL".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                match reqwest::get(&url).await {
                    Ok(resp) => {
                        let status = resp.status();
                        let body = resp
                            .text()
                            .await
                            .unwrap_or_else(|_| "Error reading body".to_string());
                        println!("Fetch {}: {} - {}", url, status, body);
                        Ok(Value::from_bits(0))
                    }
                    Err(e) => {
                        println!("Fetch {} failed: {}", url, e);
                        Ok(Value::from_bits(0))
                    }
                }
            })
        }),
    );

    fns.insert(
        "serve".to_string(),
        Arc::new(|ctx, args, loc| {
            Box::pin(async move {
                if args.len() != 2 {
                    return Err(JitError::Runtime(
                        "serve(port, handler) expects 2 arguments".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                let port = args[0].as_number().ok_or_else(|| {
                    JitError::Runtime(
                        "serve error: port must be a number".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })? as u16;

                let handler_name = args[1].as_string(&ctx).ok_or_else(|| {
                    JitError::Runtime(
                        "serve error: handler must be a function name string".into(),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
                    .await
                    .map_err(|e| {
                        JitError::Runtime(
                            format!("Failed to bind to port {}: {}", port, e),
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?;

                println!("Web server listening on port {}", port);

                loop {
                    tokio::select! {
                        accept_res = listener.accept() => {
                            let (mut socket, _) = accept_res.map_err(|e| {
                                JitError::Runtime(
                                    format!("Accept error: {}", e),
                                    loc.line as usize,
                                    loc.col as usize,
                                )
                            })?;

                            let ctx = ctx.clone();
                            let handler_name = handler_name.clone();

                            tokio::spawn(async move {
                                println!("DEBUG HTTP: Received connection");
                                let mut buf = [0; 1024];
                                let n = match socket.read(&mut buf).await {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };

                                let req_data = String::from_utf8_lossy(&buf[..n]).to_string();
                                println!("DEBUG HTTP: Request data: {}", req_data);

                                // Find the function by name ID in the callables array
                                let name_id = ctx.string_pool.iter()
                                    .position(|s| s.as_ref() == handler_name)
                                    .map(|i| i as u32);

                                let callable = name_id.and_then(|id| ctx.get_callable(id));

                                if let Some(Callable::User(f)) = callable {
                                    println!("DEBUG HTTP: Executing handler '{}'", handler_name);
                                    let instructions = f.instructions.clone();
                                    let mut regs = Vec::with_capacity(f.locals_count);
                                    for _ in 0..f.locals_count {
                                        regs.push(AtomicU64::new(0));
                                    }
                                    let registers: Arc<[AtomicU64]> = Arc::from(regs);

                                    // Setup argument: req_data
                                    if f.locals_count > 0 {
                                        let val = if let Some(sso) = Value::sso(&req_data) {
                                            sso
                                        } else {
                                            let temp = AtomicU64::new(0);
                                            ctx.alloc(ManagedObject::String(Arc::from(req_data)), &temp);
                                            Value::from_bits(temp.load(Ordering::Relaxed))
                                        };
                                        registers[0].store(val.to_bits(), Ordering::Relaxed);
                                    }

                                    let mut js = JoinSet::new();
                                    match execute_bytecode(instructions, ctx.clone(), &mut js, registers, None).await {
                                        Ok(res) => {
                                            let resp_body = res.as_string(&ctx).unwrap_or_else(|| "OK".into());
                                            let full_resp = if resp_body.starts_with("HTTP/") {
                                                resp_body
                                            } else {
                                                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}", resp_body.len(), resp_body)
                                            };
                                            let _ = socket.write_all(full_resp.as_bytes()).await;
                                        }
                                        Err(e) => {
                                            let err_msg = format!("HTTP/1.1 500 Internal Server Error\r\n\r\nError: {:?}", e);
                                            let _ = socket.write_all(err_msg.as_bytes()).await;
                                        }
                                    }
                                } else {
                                    let _ = socket.write_all(b"HTTP/1.1 404 Not Found\r\n\r\nHandler not found").await;
                                }
                            });
                        }
                        _ = tokio::signal::ctrl_c() => {
                            println!("\nShutting down web server gracefully...");
                            break;
                        }
                    }
                }
                Ok(Value::from_bits(0))
            })
        }),
    );

    fns.insert(
        "str".to_string(),
        Arc::new(|ctx, args, loc| {
            Box::pin(async move {
                if args.len() != 1 {
                    return Err(JitError::Runtime(
                        "str() expects 1 argument".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                }
                let s = stringify_value(&ctx, args[0]);
                if let Some(val) = Value::sso(&s) {
                    Ok(val)
                } else {
                    let temp = AtomicU64::new(0);
                    ctx.alloc(ManagedObject::String(Arc::from(s)), &temp);
                    Ok(Value::from_bits(temp.load(Ordering::Relaxed)))
                }
            })
        }),
    );
}

fn print_value(ctx: &Context, val: Value) {
    print!("{}", stringify_value(ctx, val));
}

fn stringify_value(ctx: &Context, val: Value) -> String {
    if let Some(n) = val.as_number() {
        n.to_string()
    } else if let Some(b) = val.as_bool() {
        b.to_string()
    } else if let Some(s) = val.as_string(ctx) {
        s
    } else if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.objects.read().unwrap();
        if let Some(Some(crate::backends::HeapObject { obj, .. })) = heap.get(oid as usize) {
            match obj {
                ManagedObject::String(s) => s.to_string(),
                ManagedObject::List(elements) => {
                    let mut res = String::from("[");
                    for (i, atomic_v) in elements.iter().enumerate() {
                        if i > 0 {
                            res.push_str(", ");
                        }
                        let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                        res.push_str(&stringify_value_nested(ctx, v));
                    }
                    res.push(']');
                    res
                }
            }
        } else {
            "null".to_string()
        }
    } else {
        "unknown".to_string()
    }
}

fn stringify_value_nested(ctx: &Context, val: Value) -> String {
    if let Some(s) = val.as_string(ctx) {
        format!("\"{}\"", s)
    } else if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.objects.read().unwrap();
        if let Some(Some(crate::backends::HeapObject { obj, .. })) = heap.get(oid as usize) {
            match obj {
                ManagedObject::String(s) => format!("\"{}\"", s),
                ManagedObject::List(_) => "[...]".to_string(),
            }
        } else {
            "null".to_string()
        }
    } else {
        stringify_value(ctx, val)
    }
}
