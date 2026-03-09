use crate::{
    backends::{
        Backend, Callable, Context, Generation, Heap, HeapMetadata, ManagedObject, NativeFn,
    },
    compiler::{Instruction, Program, QNAN, Value},
    error::JitError,
};
use parking_lot::RwLock;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinSet;

pub struct Interpreter;

impl Backend for Interpreter {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>> {
        Box::pin(async move { run_interpreter(program).await })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Build a zeroed `AtomicU64` register array of `count` slots.
fn make_registers(count: usize) -> Arc<[AtomicU64]> {
    let vec: Vec<AtomicU64> = (0..count).map(|_| AtomicU64::new(0)).collect();
    Arc::from(vec)
}

/// Intern a function's arguments into a fresh register set, returning it as an `Arc`.
fn build_call_registers(func_locals: usize, args: &[Value]) -> Arc<[AtomicU64]> {
    let regs: Vec<AtomicU64> = (0..func_locals).map(|_| AtomicU64::new(0)).collect();
    for (i, val) in args.iter().enumerate() {
        if i < regs.len() {
            unsafe {
                regs.get_unchecked(i)
                    .store(val.to_bits(), Ordering::Relaxed)
            };
        }
    }
    Arc::from(regs)
}

/// Lookup a name in the string pool, returning a static-lifetime-compatible `&str`.
///
/// The caller must ensure `ctx` outlives the returned string slice.
#[inline]
fn pool_name(ctx: &Context, name_id: u32) -> &str {
    ctx.string_pool
        .get(name_id as usize)
        .map(|s| s.as_ref())
        .unwrap_or("")
}

// ---------------------------------------------------------------------------
// Interpreter entry point
// ---------------------------------------------------------------------------

async fn run_interpreter(program: Program) -> Result<(), JitError> {
    let globals: Arc<[AtomicU64]> = make_registers(program.globals_count);

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
            string_pool_vec.push(Arc::from(name.as_str()));
            string_pool_index.insert(name.clone(), id);
        }
    }

    // Initial heap objects: strings are tenured from birth.
    let heap_init: Vec<_> = string_pool_vec
        .iter()
        .map(|s| {
            Some(crate::backends::HeapObject {
                obj: ManagedObject::String(s.clone()),
                last_gc_id: 0,
                generation: Generation::Tenured,
            })
        })
        .collect();

    // Unified callables array indexed by string pool ID.
    let mut callables_vec: Vec<Option<Callable>> = vec![None; string_pool_vec.len()];

    for func in program.functions.iter() {
        if (func.name_id as usize) < callables_vec.len() {
            callables_vec[func.name_id as usize] = Some(Callable::User(func.clone()));
        }
    }
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
            objects: RwLock::new(heap_init),
            metadata: std::sync::Mutex::new(HeapMetadata {
                free_list: Vec::new(),
                nursery_ids: Vec::new(),
                remembered_set: rustc_hash::FxHashSet::default(),
            }),
            gc_count: std::sync::atomic::AtomicU32::new(0),
            alloc_since_gc: std::sync::atomic::AtomicUsize::new(0),
        },
    });

    let registers = make_registers(program.locals_count);
    let mut join_set = JoinSet::new();
    let task_roots = Arc::new(Mutex::new(Vec::with_capacity(32)));

    ctx.active_registers
        .lock()
        .unwrap()
        .push(task_roots.clone());

    execute_bytecode(
        &program.instructions,
        ctx.clone(),
        &mut join_set,
        registers,
        None,
        task_roots,
    )
    .await?;

    drain_join_set(&mut join_set).await
}

/// Drain a `JoinSet`, propagating the first error encountered.
async fn drain_join_set(join_set: &mut JoinSet<Result<(), JitError>>) -> Result<(), JitError> {
    while let Some(res) = join_set.join_next().await {
        if let Ok(Err(e)) = res {
            return Err(e);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Instruction dispatch
// ---------------------------------------------------------------------------

/// The principal execution loop of the YatsuScript virtual machine.
///
/// This VM is register-based, meaning instructions operate on a fixed-size
/// array of `registers` (each an `AtomicU64` containing a NaN-boxed `Value`).
///
/// # GC Integration
/// Registers must be registered with the `task_roots` to ensure the garbage
/// collector can trace and protect live objects during collection cycles.
#[async_recursion::async_recursion]
pub async fn execute_bytecode(
    instructions: &Arc<[Instruction]>,
    ctx: Arc<Context>,
    join_set: &mut JoinSet<Result<(), JitError>>,
    registers: Arc<[AtomicU64]>,
    dst_reg: Option<&AtomicU64>,
    task_roots: Arc<Mutex<Vec<Arc<[AtomicU64]>>>>,
) -> Result<Value, JitError> {
    // Register the current local register set for GC tracing.
    task_roots.lock().unwrap().push(registers.clone());

    // RAII guard: de-register when this scope exits.
    struct RegGuard(Arc<Mutex<Vec<Arc<[AtomicU64]>>>>);
    impl Drop for RegGuard {
        fn drop(&mut self) {
            self.0.lock().unwrap().pop();
        }
    }
    let _guard = RegGuard(task_roots.clone());

    // --- Internal VM register helpers ---

    #[inline(always)]
    fn load(regs: &[AtomicU64], index: usize) -> Value {
        Value::from_bits(unsafe { regs.get_unchecked(index).load(Ordering::Relaxed) })
    }

    #[inline(always)]
    fn store(regs: &[AtomicU64], index: usize, val: Value) {
        unsafe {
            regs.get_unchecked(index)
                .store(val.to_bits(), Ordering::Relaxed)
        };
    }

    // Numeric binary operation with fast-path for plain f64 bits.
    macro_rules! binary_op {
        ($dst:expr, $lhs:expr, $rhs:expr, $op:tt, $loc:expr) => {{
            let l_bits = unsafe { registers.get_unchecked(*$lhs).load(Ordering::Relaxed) };
            let r_bits = unsafe { registers.get_unchecked(*$rhs).load(Ordering::Relaxed) };

            if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                store(&registers, *$dst, Value::number(f64::from_bits(l_bits) $op f64::from_bits(r_bits)));
            } else {
                let l = Value::from_bits(l_bits);
                let r = Value::from_bits(r_bits);
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    store(&registers, *$dst, Value::number(lv $op rv));
                } else {
                    return Err(JitError::runtime(
                        format!("Math error: expected numbers for '{}'", stringify!($op)),
                        $loc.line as usize,
                        $loc.col as usize,
                    ));
                }
            }
        }};
    }

    // Numeric comparison with fast-path for plain f64 bits.
    macro_rules! compare_op {
        ($dst:expr, $lhs:expr, $rhs:expr, $op:tt, $loc:expr) => {{
            let l_bits = unsafe { registers.get_unchecked(*$lhs).load(Ordering::Relaxed) };
            let r_bits = unsafe { registers.get_unchecked(*$rhs).load(Ordering::Relaxed) };

            let result = if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                Some(f64::from_bits(l_bits) $op f64::from_bits(r_bits))
            } else {
                let l = Value::from_bits(l_bits);
                let r = Value::from_bits(r_bits);
                if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) {
                    Some(lv $op rv)
                } else {
                    None
                }
            };

            match result {
                Some(b) => store(&registers, *$dst, Value::bool(b)),
                None => return Err(JitError::runtime(
                    format!("Compare error: expected numbers for '{}'", stringify!($op)),
                    $loc.line as usize,
                    $loc.col as usize,
                )),
            }
        }};
    }

    // Perform a CAS-based increment on any AtomicU64 holding a NaN-boxed number.
    macro_rules! atomic_increment {
        ($ptr:expr) => {{
            let mut old_bits = $ptr.load(Ordering::Relaxed);
            loop {
                let next = if (old_bits & QNAN) != QNAN {
                    Value::number(f64::from_bits(old_bits) + 1.0)
                } else if let Some(n) = Value::from_bits(old_bits).as_number() {
                    Value::number(n + 1.0)
                } else {
                    break; // Not a number; silently skip
                };

                match $ptr.compare_exchange_weak(
                    old_bits,
                    next.to_bits(),
                    Ordering::AcqRel,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => break,
                    Err(actual) => old_bits = actual,
                }
            }
        }};
    }

    // Invoke a `Callable` (either native or user) and optionally store the result.
    macro_rules! dispatch_call {
        ($callable:expr, $args_vals:expr, $dst:expr, $loc:expr) => {{
            match $callable {
                Callable::Native(native_fn) => {
                    let res = native_fn(ctx.clone(), $args_vals, $loc).await?;
                    if let Some(dst_idx) = $dst {
                        store(&registers, dst_idx, res);
                    }
                }
                Callable::User(func) => {
                    let f_regs = build_call_registers(func.locals_count, &$args_vals);
                    let _ = execute_bytecode(
                        &func.instructions,
                        ctx.clone(),
                        join_set,
                        f_regs,
                        ($dst).map(|r| unsafe { registers.get_unchecked(r) }),
                        task_roots.clone(),
                    )
                    .await?;
                }
            }
        }};
    }

    // Equality check shared by `Eq` and `Ne`.
    #[inline(always)]
    fn values_equal_fast(ctx: &Context, l_bits: u64, r_bits: u64) -> bool {
        if l_bits == r_bits && (l_bits & QNAN) != QNAN {
            true
        } else {
            ctx.values_equal(Value::from_bits(l_bits), Value::from_bits(r_bits))
        }
    }

    let mut pc = 0usize;
    let mut instr_count: u32 = 0;

    // --- Main Dispatch Loop ---
    while pc < instructions.len() {
        let instr = unsafe { instructions.get_unchecked(pc) };
        instr_count = instr_count.wrapping_add(1);

        match instr {
            Instruction::LoadLiteral { dst, val } => {
                store(&registers, *dst, *val);
            }
            Instruction::Move { dst, src } => {
                store(&registers, *dst, load(&registers, *src));
            }
            Instruction::LoadGlobal { dst, global } => {
                store(&registers, *dst, load(&ctx.globals, *global));
            }
            Instruction::StoreGlobal { global, src } => {
                let val = load(&registers, *src);
                unsafe {
                    ctx.globals
                        .get_unchecked(*global)
                        .store(val.to_bits(), Ordering::Relaxed);
                }
            }

            // --- Calls ---
            Instruction::Call(box_data) => {
                let crate::compiler::CallData {
                    name_id,
                    args_regs,
                    dst,
                    loc,
                } = &**box_data;
                let callable = ctx.get_callable(*name_id).ok_or_else(|| {
                    let name = unsafe { ctx.string_pool.get_unchecked(*name_id as usize) };
                    JitError::runtime(
                        format!("Unknown function: {}", name),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                // For user functions, verify arity before building registers.
                if let Callable::User(func) = callable
                    && args_regs.len() != func.params_count {
                        return Err(JitError::runtime(
                            format!(
                                "Function arity mismatch: expected {}, got {}",
                                func.params_count,
                                args_regs.len()
                            ),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }

                let args: Vec<Value> = args_regs.iter().map(|&r| load(&registers, r)).collect();
                dispatch_call!(callable, args, dst.map(|r| r), *loc);
            }

            Instruction::CallDynamic(box_data) => {
                let crate::compiler::CallDynamicData {
                    callee_reg,
                    args_regs,
                    dst,
                    loc,
                } = &**box_data;
                let callee_val = load(&registers, *callee_reg);
                let args: Vec<Value> = args_regs.iter().map(|&r| load(&registers, r)).collect();

                // Handle BoundMethod (e.g., range.step(...) or list.pad(...))
                if let Some(oid) = callee_val.as_obj_id() {
                    let heap = ctx.heap.objects.read();
                    if let Some(Some(obj_ref)) = heap.get(oid as usize)
                        && let ManagedObject::BoundMethod { receiver, name_id } = &obj_ref.obj {
                            let method = pool_name(&ctx, *name_id).to_owned();
                            let receiver_val = *receiver;
                            drop(heap);

                            if method == "pad" {
                                // list.pad(n, value) — resize the list to n elements,
                                // filling any new slots with the given value.
                                if let Some(list_oid) = receiver_val.as_obj_id() {
                                    let n = args.first().and_then(|v| v.as_number()).unwrap_or(0.0)
                                        as usize;
                                    let fill_bits = args
                                        .get(1)
                                        .copied()
                                        .unwrap_or_else(|| Value::from_bits(0))
                                        .to_bits();
                                    let heap = ctx.heap.objects.read();
                                    if let Some(Some(list_obj)) = heap.get(list_oid as usize)
                                        && let ManagedObject::List(elements) = &list_obj.obj {
                                            let mut w = elements.write();
                                            if w.len() < n {
                                                w.resize_with(n, || AtomicU64::new(fill_bits));
                                            }
                                        }
                                }
                                pc += 1;
                                continue;
                            } else if method == "step"
                                && let Some(r_oid) = receiver_val.as_obj_id() {
                                    // Extract start/end while holding the read-lock, then drop
                                    // it before calling ctx.alloc(), which needs the write-lock.
                                    let range_vals = {
                                        let heap = ctx.heap.objects.read();
                                        if let Some(Some(r_obj)) = heap.get(r_oid as usize) {
                                            if let ManagedObject::Range { start, end, .. } =
                                                &r_obj.obj
                                            {
                                                Some((*start, *end))
                                            } else {
                                                None
                                            }
                                        } else {
                                            None
                                        }
                                    }; // <-- read-lock dropped here

                                    if let Some((start, end)) = range_vals {
                                        let new_step =
                                            args.first().and_then(|v| v.as_number()).unwrap_or(1.0);
                                        let temp = AtomicU64::new(0);
                                        ctx.alloc(
                                            ManagedObject::Range {
                                                start,
                                                end,
                                                step: new_step,
                                            },
                                            &temp,
                                        );
                                        if let Some(dst_idx) = dst {
                                            store(
                                                &registers,
                                                *dst_idx,
                                                Value::from_bits(temp.load(Ordering::Relaxed)),
                                            );
                                        }
                                        pc += 1;
                                        continue;
                                    }
                                }
                            // Unknown bound method — fall through to produce a clear error.
                            return Err(JitError::runtime(
                                format!("Unknown method '{}'", method),
                                loc.line as usize,
                                loc.col as usize,
                            ));
                        }
                }

                let name_id = ctx.value_as_pool_id(callee_val).ok_or_else(|| {
                    JitError::runtime(
                        "Callee is not a known function name",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                let callable = ctx.get_callable(name_id).ok_or_else(|| {
                    JitError::runtime(
                        format!(
                            "Dynamic call: unknown function '{}'",
                            pool_name(&ctx, name_id)
                        ),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                dispatch_call!(callable, args, dst.map(|r| r), *loc);
            }

            // --- Control flow ---
            Instruction::Return(val_reg) => {
                let val = val_reg
                    .map(|r| load(&registers, r))
                    .unwrap_or_else(|| Value::from_bits(0));
                if let Some(dst) = dst_reg {
                    dst.store(val.to_bits(), Ordering::Relaxed);
                }
                return Ok(val);
            }
            Instruction::Jump(target) => {
                pc = *target;
                continue;
            }
            Instruction::JumpIfFalse { cond, target } => {
                if !load(&registers, *cond).is_truthy() {
                    pc = *target;
                    continue;
                }
            }

            // --- Ranges ---
            Instruction::Range {
                dst,
                start,
                end,
                step,
                loc,
            } => {
                let start_val = load(&registers, *start).as_number().ok_or_else(|| {
                    JitError::runtime(
                        "Range start must be a number",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                let end_val = load(&registers, *end).as_number().ok_or_else(|| {
                    JitError::runtime(
                        "Range end must be a number",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                let step_val = if let Some(step_reg) = *step {
                    load(&registers, step_reg).as_number().ok_or_else(|| {
                        JitError::runtime(
                            "Range step must be a number",
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?
                } else {
                    1.0
                };

                ctx.alloc(
                    ManagedObject::Range {
                        start: start_val,
                        end: end_val,
                        step: step_val,
                    },
                    unsafe { registers.get_unchecked(*dst) },
                );
            }
            Instruction::RangeInfo {
                range,
                start_dst,
                end_dst,
                step_dst,
            } => {
                let range_val = load(&registers, *range);
                let (s, e, st) = if let Some(oid) = range_val.as_obj_id() {
                    let heap = ctx.heap.objects.read();
                    if let Some(Some(obj_ref)) = heap.get(oid as usize) {
                        if let ManagedObject::Range { start, end, step } = &obj_ref.obj {
                            (*start, *end, *step)
                        } else {
                            (0.0, 0.0, 1.0)
                        }
                    } else {
                        (0.0, 0.0, 1.0)
                    }
                } else {
                    (0.0, 0.0, 1.0)
                };
                store(&registers, *start_dst, Value::number(s));
                store(&registers, *end_dst, Value::number(e));
                store(&registers, *step_dst, Value::number(st));
            }

            // --- Arithmetic ---
            Instruction::Not { dst, src, .. } => {
                store(
                    &registers,
                    *dst,
                    Value::bool(!load(&registers, *src).is_truthy()),
                );
            }
            Instruction::Add { dst, lhs, rhs, loc } => {
                let l_val = load(&registers, *lhs);
                let r_val = load(&registers, *rhs);
                let l_bits = l_val.to_bits();
                let r_bits = r_val.to_bits();

                if (l_bits & QNAN) != QNAN && (r_bits & QNAN) != QNAN {
                    // Fast path: plain f64 + f64
                    store(
                        &registers,
                        *dst,
                        Value::number(f64::from_bits(l_bits) + f64::from_bits(r_bits)),
                    );
                } else if let (Some(lv), Some(rv)) = (l_val.as_number(), r_val.as_number()) {
                    store(&registers, *dst, Value::number(lv + rv));
                } else {
                    // String concatenation
                    let combined = l_val
                        .with_str(&ctx, |l_str| {
                            r_val.with_str(&ctx, |r_str| {
                                let mut s = String::with_capacity(l_str.len() + r_str.len());
                                s.push_str(l_str);
                                s.push_str(r_str);
                                s
                            })
                        })
                        .flatten();

                    match combined {
                        Some(s) if Value::sso(&s).is_some() => {
                            store(&registers, *dst, Value::sso(&s).unwrap());
                        }
                        Some(s) => {
                            ctx.alloc(ManagedObject::String(Arc::from(s)), unsafe {
                                registers.get_unchecked(*dst)
                            });
                        }
                        None => {
                            return Err(JitError::runtime(
                                "Add error: expected numbers or strings",
                                loc.line as usize,
                                loc.col as usize,
                            ));
                        }
                    }
                }
            }
            Instruction::Sub { dst, lhs, rhs, loc } => binary_op!(dst, lhs, rhs, -, loc),
            Instruction::Mul { dst, lhs, rhs, loc } => binary_op!(dst, lhs, rhs, *, loc),
            Instruction::Div { dst, lhs, rhs, loc } => binary_op!(dst, lhs, rhs, /, loc),

            // --- Increment ---
            Instruction::Increment(reg) => {
                atomic_increment!(unsafe { registers.get_unchecked(*reg) });
            }
            Instruction::IncrementGlobal(global) => {
                atomic_increment!(unsafe { ctx.globals.get_unchecked(*global) });
            }

            // --- Equality / Comparison ---
            Instruction::Eq { dst, lhs, rhs } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                store(
                    &registers,
                    *dst,
                    Value::bool(values_equal_fast(&ctx, l_bits, r_bits)),
                );
            }
            Instruction::Ne { dst, lhs, rhs } => {
                let l_bits = unsafe { registers.get_unchecked(*lhs).load(Ordering::Relaxed) };
                let r_bits = unsafe { registers.get_unchecked(*rhs).load(Ordering::Relaxed) };
                store(
                    &registers,
                    *dst,
                    Value::bool(!values_equal_fast(&ctx, l_bits, r_bits)),
                );
            }
            Instruction::Lt { dst, lhs, rhs, loc } => compare_op!(dst, lhs, rhs, <,  loc),
            Instruction::Le { dst, lhs, rhs, loc } => compare_op!(dst, lhs, rhs, <=, loc),
            Instruction::Gt { dst, lhs, rhs, loc } => compare_op!(dst, lhs, rhs, >,  loc),
            Instruction::Ge { dst, lhs, rhs, loc } => compare_op!(dst, lhs, rhs, >=, loc),

            // --- Lists ---
            Instruction::NewList { dst, len } => {
                let elements: Vec<AtomicU64> = (0..*len).map(|_| AtomicU64::new(0)).collect();
                ctx.alloc(ManagedObject::List(RwLock::new(elements)), unsafe {
                    registers.get_unchecked(*dst)
                });
            }
            Instruction::ListGet {
                dst,
                list,
                index_reg,
                loc,
            } => {
                let list_val = load(&registers, *list);
                let index = load(&registers, *index_reg)
                    .as_number()
                    .map(|n| n as usize)
                    .ok_or_else(|| {
                        JitError::runtime(
                            "List index must be a number",
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?;

                let oid = list_val.as_obj_id().ok_or_else(|| {
                    JitError::runtime(
                        "Expected list for indexing",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                let heap = ctx.heap.objects.read();
                let obj = heap
                    .get(oid as usize)
                    .and_then(|o| o.as_ref())
                    .ok_or_else(|| {
                        JitError::runtime(
                            "Expected list for indexing",
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?;
                let ManagedObject::List(elements) = &obj.obj else {
                    return Err(JitError::runtime(
                        "Expected list for indexing",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };
                let lock = elements.read();
                let slot = lock.get(index).ok_or_else(|| {
                    JitError::runtime(
                        format!(
                            "Index out of bounds: {} for list of length {}",
                            index,
                            lock.len()
                        ),
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                unsafe {
                    registers
                        .get_unchecked(*dst)
                        .store(slot.load(Ordering::Relaxed), Ordering::Relaxed);
                }
            }
            Instruction::ListSet {
                list,
                index_reg,
                src,
                loc,
            } => {
                let list_val = load(&registers, *list);
                let index_bits =
                    unsafe { registers.get_unchecked(*index_reg).load(Ordering::Relaxed) };
                let src_bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };

                let index = if (index_bits & QNAN) != QNAN {
                    f64::from_bits(index_bits) as usize
                } else {
                    Value::from_bits(index_bits)
                        .as_number()
                        .map(|n| n as usize)
                        .ok_or_else(|| {
                            JitError::runtime(
                                "List index must be a number",
                                loc.line as usize,
                                loc.col as usize,
                            )
                        })?
                };

                let oid = list_val.as_obj_id().ok_or_else(|| {
                    JitError::runtime(
                        "Expected list for indexing",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                let heap = ctx.heap.objects.read();
                let obj = heap
                    .get(oid as usize)
                    .and_then(|o| o.as_ref())
                    .ok_or_else(|| {
                        JitError::runtime(
                            "Expected list for indexing",
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?;
                let ManagedObject::List(elements) = &obj.obj else {
                    return Err(JitError::runtime(
                        "Expected list for indexing",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };

                // Grow the list if needed (double-checked to avoid data race).
                if elements.read().len() <= index {
                    let mut write = elements.write();
                    if write.len() <= index {
                        write.resize_with(index + 1, Default::default);
                    }
                    write[index].store(src_bits, Ordering::Relaxed);
                } else {
                    elements.read()[index].store(src_bits, Ordering::Relaxed);
                }

                // Write barrier: track tenured → nursery references.
                if obj.generation == Generation::Tenured && (src_bits & QNAN) == QNAN
                    && let Some(src_oid) = Value::from_bits(src_bits).as_obj_id() {
                        let heap2 = ctx.heap.objects.read();
                        if let Some(Some(src_obj)) = heap2.get(src_oid as usize)
                            && src_obj.generation == Generation::Nursery {
                                ctx.heap.metadata.lock().unwrap().remembered_set.insert(oid);
                            }
                    }
            }

            // --- Objects ---
            Instruction::NewObject { dst, capacity } => {
                let fields =
                    rustc_hash::FxHashMap::with_capacity_and_hasher(*capacity, Default::default());
                ctx.alloc(ManagedObject::Object(RwLock::new(fields)), unsafe {
                    registers.get_unchecked(*dst)
                });
            }
            Instruction::ObjectGet {
                dst,
                obj,
                name_id,
                loc,
            } => {
                let obj_val = load(&registers, *obj);

                enum GetResult {
                    Val(Value),
                    Method(Value, u32),
                    Error(&'static str),
                }

                let get_result = if let Some(oid) = obj_val.as_obj_id() {
                    let heap = ctx.heap.objects.read();
                    if let Some(Some(obj_ref)) = heap.get(oid as usize) {
                        match &obj_ref.obj {
                            ManagedObject::Object(fields) => {
                                let fields = fields.read();
                                let val = fields
                                    .get(name_id)
                                    .map(|s| Value::from_bits(s.load(Ordering::Relaxed)))
                                    .unwrap_or_else(|| Value::from_bits(0));
                                GetResult::Val(val)
                            }
                            ManagedObject::Timestamp(start) => {
                                let val = if pool_name(&ctx, *name_id) == "elapsed" {
                                    Value::number(start.elapsed().as_secs_f64())
                                } else {
                                    Value::from_bits(0)
                                };
                                GetResult::Val(val)
                            }
                            ManagedObject::List(_) => {
                                if pool_name(&ctx, *name_id) == "pad" {
                                    GetResult::Method(obj_val, *name_id)
                                } else {
                                    GetResult::Val(Value::from_bits(0))
                                }
                            }
                            ManagedObject::Range { start, end, .. } => {
                                
                                match pool_name(&ctx, *name_id) {
                                    "start" => GetResult::Val(Value::number(*start)),
                                    "end" => GetResult::Val(Value::number(*end)),
                                    "step" => GetResult::Method(obj_val, *name_id),
                                    _ => GetResult::Val(Value::from_bits(0)),
                                }
                            }
                            _ => GetResult::Error("Expected object for property access"),
                        }
                    } else {
                        GetResult::Error("Expected object for property access")
                    }
                } else {
                    GetResult::Error("Expected object for property access")
                };

                match get_result {
                    GetResult::Val(v) => store(&registers, *dst, v),
                    GetResult::Method(recv, nid) => {
                        let temp = AtomicU64::new(0);
                        ctx.alloc(
                            ManagedObject::BoundMethod {
                                receiver: recv,
                                name_id: nid,
                            },
                            &temp,
                        );
                        store(
                            &registers,
                            *dst,
                            Value::from_bits(temp.load(Ordering::Relaxed)),
                        );
                    }
                    GetResult::Error(msg) => {
                        return Err(JitError::runtime(msg, loc.line as usize, loc.col as usize));
                    }
                }
            }
            Instruction::ObjectSet {
                obj,
                name_id,
                src,
                loc,
            } => {
                let obj_bits = unsafe { registers.get_unchecked(*obj).load(Ordering::Relaxed) };
                let src_bits = unsafe { registers.get_unchecked(*src).load(Ordering::Relaxed) };
                let obj_val = Value::from_bits(obj_bits);

                let oid = obj_val.as_obj_id().ok_or_else(|| {
                    JitError::runtime(
                        "Expected object for property assignment",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                let heap = ctx.heap.objects.read();
                let obj_ref = heap
                    .get(oid as usize)
                    .and_then(|o| o.as_ref())
                    .ok_or_else(|| {
                        JitError::runtime(
                            "Expected object for property assignment",
                            loc.line as usize,
                            loc.col as usize,
                        )
                    })?;
                let ManagedObject::Object(fields) = &obj_ref.obj else {
                    return Err(JitError::runtime(
                        "Expected object for property assignment",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };

                // Insert or update field (prefer read-lock update when slot exists).
                {
                    let fields_read = fields.read();
                    if let Some(slot) = fields_read.get(name_id) {
                        slot.store(src_bits, Ordering::Relaxed);
                    } else {
                        drop(fields_read);
                        fields.write().insert(*name_id, AtomicU64::new(src_bits));
                    }
                }

                // Write barrier.
                if obj_ref.generation == Generation::Tenured && (src_bits & QNAN) == QNAN
                    && let Some(src_oid) = Value::from_bits(src_bits).as_obj_id()
                        && let Some(Some(src_obj)) = heap.get(src_oid as usize)
                            && src_obj.generation == Generation::Nursery {
                                ctx.heap.metadata.lock().unwrap().remembered_set.insert(oid);
                            }
            }

            // --- Concurrency ---
            Instruction::Spawn(box_data) => {
                let crate::compiler::SpawnData {
                    instructions: t_instrs,
                    locals_count,
                    captures,
                } = &**box_data;
                let body = Arc::clone(t_instrs);
                let s_ctx = ctx.clone();

                let t_regs: Vec<AtomicU64> =
                    (0..*locals_count).map(|_| AtomicU64::new(0)).collect();
                for &reg in captures.iter() {
                    let bits = unsafe { registers.get_unchecked(reg).load(Ordering::Relaxed) };
                    t_regs[reg].store(bits, Ordering::Relaxed);
                }
                let thread_regs: Arc<[AtomicU64]> = Arc::from(t_regs);

                join_set.spawn(async move {
                    let mut js = JoinSet::new();
                    let t_roots = Arc::new(Mutex::new(Vec::with_capacity(16)));
                    s_ctx.active_registers.lock().unwrap().push(t_roots.clone());

                    let res =
                        execute_bytecode(&body.clone(), s_ctx, &mut js, thread_regs, None, t_roots)
                            .await;
                    drain_join_set(&mut js).await?;
                    res.map(|_| ())
                });
            }
        }

        pc += 1;
        if instr_count & 0x3FFF == 0 {
            tokio::task::yield_now().await;
        }
    }
    Ok(Value::from_bits(0))
}

// ---------------------------------------------------------------------------
// Native function registry
// ---------------------------------------------------------------------------

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
                let [val] = args.as_slice() else {
                    return Err(JitError::runtime(
                        "len() expects 1 argument",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };
                let val = *val;
                if let Some(oid) = val.as_obj_id() {
                    let heap = ctx.heap.objects.read();
                    if let Some(Some(obj)) = heap.get(oid as usize) {
                        return Ok(Value::number(match &obj.obj {
                            ManagedObject::String(s) => s.len() as f64,
                            ManagedObject::List(l) => l.read().len() as f64,
                            ManagedObject::Object(o) => o.read().len() as f64,
                            ManagedObject::Timestamp(_) => 0.0,
                            ManagedObject::Range { start, end, step } => {
                                if *step == 0.0 {
                                    0.0
                                } else {
                                    ((end - start) / step).ceil().max(0.0)
                                }
                            }
                            ManagedObject::BoundMethod { .. } => 0.0,
                        }));
                    }
                } else if let Some(s) = val.as_string(&ctx) {
                    return Ok(Value::number(s.len() as f64));
                }
                Err(JitError::runtime(
                    "len() expects string or list",
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
        "timestamp".to_string(),
        Arc::new(|ctx, _, _| {
            Box::pin(async move {
                let temp = AtomicU64::new(0);
                ctx.alloc(ManagedObject::Timestamp(std::time::Instant::now()), &temp);
                Ok(Value::from_bits(temp.load(Ordering::Relaxed)))
            })
        }),
    );

    fns.insert(
        "sleep".to_string(),
        Arc::new(|_, args, loc| {
            Box::pin(async move {
                let [val] = args.as_slice() else {
                    return Err(JitError::runtime(
                        "sleep() expects 1 argument",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };
                let ms = val.as_number().ok_or_else(|| {
                    JitError::runtime(
                        "sleep() expects numeric milliseconds",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;
                tokio::time::sleep(tokio::time::Duration::from_millis(ms as u64)).await;
                Ok(Value::from_bits(0))
            })
        }),
    );

    fns.insert(
        "fetch".to_string(),
        Arc::new(|ctx, args, loc| {
            Box::pin(async move {
                let url = args
                    .first()
                    .and_then(|v| v.as_string(&ctx))
                    .ok_or_else(|| {
                        JitError::runtime(
                            "fetch requires a string URL as its first argument",
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
                let (port_val, handler_val) = match args.as_slice() {
                    [p, h] => (*p, *h),
                    _ => {
                        return Err(JitError::runtime(
                            "serve(port, handler) expects 2 arguments",
                            loc.line as usize,
                            loc.col as usize,
                        ))
                    }
                };

                let port = port_val.as_number().ok_or_else(|| {
                    JitError::runtime("serve: port must be a number", loc.line as usize, loc.col as usize)
                })? as u16;

                let handler_name = handler_val.as_string(&ctx).ok_or_else(|| {
                    JitError::runtime(
                        "serve: handler must be a function name string",
                        loc.line as usize,
                        loc.col as usize,
                    )
                })?;

                let listener = TcpListener::bind(format!("0.0.0.0:{}", port))
                    .await
                    .map_err(|e| {
                        JitError::runtime(
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
                                JitError::runtime(
                                    format!("Accept error: {}", e),
                                    loc.line as usize,
                                    loc.col as usize,
                                )
                            })?;

                            let ctx = ctx.clone();
                            let handler_name = handler_name.clone();

                            tokio::spawn(async move {
                                println!("DEBUG HTTP: Received connection");
                                let mut buf = [0u8; 1024];
                                let n = match socket.read(&mut buf).await {
                                    Ok(n) if n > 0 => n,
                                    _ => return,
                                };

                                let req_data = String::from_utf8_lossy(&buf[..n]).to_string();
                                println!("DEBUG HTTP: Request data: {}", req_data);

                                // Resolve handler by name.
                                let name_id = ctx
                                    .string_pool
                                    .iter()
                                    .position(|s| s.as_ref() == handler_name)
                                    .map(|i| i as u32);
                                let callable = name_id.and_then(|id| ctx.get_callable(id));

                                let Some(Callable::User(f)) = callable else {
                                    let _ = socket
                                        .write_all(b"HTTP/1.1 404 Not Found\r\n\r\nHandler not found")
                                        .await;
                                    return;
                                };

                                println!("DEBUG HTTP: Executing handler '{}'", handler_name);

                                let registers = make_registers(f.locals_count);
                                if f.locals_count > 0 {
                                    let val = if let Some(sso) = Value::sso(&req_data) {
                                        sso
                                    } else {
                                        let temp = AtomicU64::new(0);
                                        ctx.alloc(
                                            ManagedObject::String(Arc::from(req_data)),
                                            &temp,
                                        );
                                        Value::from_bits(temp.load(Ordering::Relaxed))
                                    };
                                    registers[0].store(val.to_bits(), Ordering::Relaxed);
                                }

                                let mut js = JoinSet::new();
                                let t_roots = Arc::new(Mutex::new(Vec::with_capacity(16)));
                                ctx.active_registers
                                    .lock()
                                    .unwrap()
                                    .push(t_roots.clone());

                                match execute_bytecode(
                                    &f.instructions,
                                    ctx.clone(),
                                    &mut js,
                                    registers,
                                    None,
                                    t_roots,
                                )
                                .await
                                {
                                    Ok(res) => {
                                        let resp_body =
                                            res.as_string(&ctx).unwrap_or_else(|| "OK".to_string());
                                        let full_resp = if resp_body.starts_with("HTTP/") {
                                            resp_body
                                        } else {
                                            format!(
                                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nContent-Type: text/plain\r\n\r\n{}",
                                                resp_body.len(),
                                                resp_body
                                            )
                                        };
                                        let _ = socket.write_all(full_resp.as_bytes()).await;
                                    }
                                    Err(e) => {
                                        let err_msg = format!(
                                            "HTTP/1.1 500 Internal Server Error\r\n\r\nError: {:?}",
                                            e
                                        );
                                        let _ = socket.write_all(err_msg.as_bytes()).await;
                                    }
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
                let [val] = args.as_slice() else {
                    return Err(JitError::runtime(
                        "str() expects 1 argument",
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };
                let s = stringify_value(&ctx, *val);
                if let Some(sso) = Value::sso(&s) {
                    Ok(sso)
                } else {
                    let temp = AtomicU64::new(0);
                    ctx.alloc(ManagedObject::String(Arc::from(s)), &temp);
                    Ok(Value::from_bits(temp.load(Ordering::Relaxed)))
                }
            })
        }),
    );
}

// ---------------------------------------------------------------------------
// Value display helpers
// ---------------------------------------------------------------------------

fn print_value(ctx: &Context, val: Value) {
    print!("{}", stringify_value(ctx, val));
}

fn stringify_value(ctx: &Context, val: Value) -> String {
    if let Some(n) = val.as_number() {
        return n.to_string();
    }
    if let Some(b) = val.as_bool() {
        return b.to_string();
    }
    if let Some(s) = val.as_string(ctx) {
        return s;
    }
    if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.objects.read();
        if let Some(Some(crate::backends::HeapObject { obj, .. })) = heap.get(oid as usize) {
            return match obj {
                ManagedObject::String(s) => s.to_string(),
                ManagedObject::List(elements) => {
                    let lock = elements.read();
                    let items: Vec<String> = lock
                        .iter()
                        .map(|a| {
                            stringify_value_nested(ctx, Value::from_bits(a.load(Ordering::Relaxed)))
                        })
                        .collect();
                    format!("[{}]", items.join(", "))
                }
                ManagedObject::Object(fields) => {
                    let fields = fields.read();
                    let entries: Vec<String> = fields
                        .iter()
                        .map(|(&name_id, atomic_val)| {
                            let name = pool_name(ctx, name_id);
                            let v = Value::from_bits(atomic_val.load(Ordering::Relaxed));
                            format!("{}: {}", name, stringify_value_nested(ctx, v))
                        })
                        .collect();
                    format!("{{{}}}", entries.join(", "))
                }
                ManagedObject::Timestamp(t) => format!("Timestamp({:?})", t),
                ManagedObject::Range { start, end, step } => {
                    if *step == 1.0 {
                        format!("{}..{}", start, end)
                    } else {
                        format!("{}..{}.step({})", start, end, step)
                    }
                }
                ManagedObject::BoundMethod { receiver, name_id } => {
                    format!(
                        "<bound method {} of {}>",
                        pool_name(ctx, *name_id),
                        stringify_value(ctx, *receiver)
                    )
                }
            };
        }
        return "null".to_string();
    }
    "unknown".to_string()
}

fn stringify_value_nested(ctx: &Context, val: Value) -> String {
    if let Some(s) = val.as_string(ctx) {
        return format!("\"{}\"", s);
    }
    if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.objects.read();
        if let Some(Some(crate::backends::HeapObject { obj, .. })) = heap.get(oid as usize) {
            return match obj {
                ManagedObject::String(s) => format!("\"{}\"", s),
                ManagedObject::List(_) => "[...]".to_string(),
                ManagedObject::Object(_) => "{...}".to_string(),
                ManagedObject::Timestamp(_) => "Timestamp(...)".to_string(),
                ManagedObject::Range { .. } => "Range(...)".to_string(),
                ManagedObject::BoundMethod { .. } => "BoundMethod(...)".to_string(),
            };
        }
        return "null".to_string();
    }
    stringify_value(ctx, val)
}
