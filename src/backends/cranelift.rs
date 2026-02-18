use crate::backends::{Backend, Context, setup_native_fns};
use crate::compiler::{Instruction, Loc, Program, Value};
use crate::error::JitError;
use cranelift::prelude::*;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::runtime::Handle;

pub struct CraneliftBackend;

impl Backend for CraneliftBackend {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>> {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || compile_and_run(program))
                .await
                .map_err(|e| JitError::Runtime(format!("Task failed: {}", e), 0, 0))?
        })
    }
}

#[repr(C)]
struct RuntimeState {
    globals_ptr: *mut u64,
    ctx_ptr: *const Context,
    error_occurred: bool,
    error_msg: *mut i8,
    error_line: u32,
    error_col: u32,
}

extern "C" fn rt_add(v1: u64, v2: u64) -> u64 {
    let l = Value::from_bits(v1); let r = Value::from_bits(v2);
    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) { Value::number(lv + rv).to_bits() } else { 0 }
}

extern "C" fn rt_sub(v1: u64, v2: u64) -> u64 {
    let l = Value::from_bits(v1); let r = Value::from_bits(v2);
    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) { Value::number(lv - rv).to_bits() } else { 0 }
}

extern "C" fn rt_mul(v1: u64, v2: u64) -> u64 {
    let l = Value::from_bits(v1); let r = Value::from_bits(v2);
    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) { Value::number(lv * rv).to_bits() } else { 0 }
}

extern "C" fn rt_div(v1: u64, v2: u64) -> u64 {
    let l = Value::from_bits(v1); let r = Value::from_bits(v2);
    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) { Value::number(lv / rv).to_bits() } else { 0 }
}

extern "C" fn rt_less_than(v1: u64, v2: u64) -> u64 {
    let l = Value::from_bits(v1); let r = Value::from_bits(v2);
    if let (Some(lv), Some(rv)) = (l.as_number(), r.as_number()) { Value::bool(lv < rv).to_bits() } else { 0 }
}

extern "C" fn rt_new_list(state_ptr: *mut RuntimeState, len: usize) -> u64 {
    let state = unsafe { &mut *state_ptr };
    let ctx = unsafe { Arc::from_raw(state.ctx_ptr) };
    let list_arc = Arc::new(std::sync::RwLock::new(vec![Value::from_bits(0); len]));
    let mut lists = ctx.lists.write().unwrap();
    let id = lists.len() as u32;
    lists.push(list_arc);
    let _ = Arc::into_raw(ctx);
    Value::list_id(id).to_bits()
}

extern "C" fn rt_list_get(state_ptr: *mut RuntimeState, list_val_bits: u64, index_val_bits: u64, line: u32, col: u32) -> u64 {
    let state = unsafe { &mut *state_ptr };
    let ctx = unsafe { Arc::from_raw(state.ctx_ptr) };
    let list_val = Value::from_bits(list_val_bits);
    let index_val = Value::from_bits(index_val_bits);
    let res = (|| {
        let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| JitError::Runtime("List index must be a number".into(), line as usize, col as usize))?;
        if let Some(lid) = list_val.as_list_id() {
            let lists = ctx.lists.read().unwrap();
            if let Some(list_arc) = lists.get(lid as usize) {
                let list = list_arc.read().unwrap();
                if let Some(val) = list.get(index) { Ok(val.to_bits()) }
                else { Err(JitError::Runtime(format!("Index out of bounds: {}", index), line as usize, col as usize)) }
            } else { Err(JitError::Runtime("Invalid list ID".into(), line as usize, col as usize)) }
        } else { Err(JitError::Runtime("Expected list".into(), line as usize, col as usize)) }
    })();
    let _ = Arc::into_raw(ctx);
    match res {
        Ok(v) => v,
        Err(e) => {
            state.error_occurred = true; state.error_line = line; state.error_col = col;
            state.error_msg = std::ffi::CString::new(e.to_string()).unwrap().into_raw();
            0
        }
    }
}

extern "C" fn rt_list_set(state_ptr: *mut RuntimeState, list_val_bits: u64, index_val_bits: u64, src_val_bits: u64, line: u32, col: u32) {
    let state = unsafe { &mut *state_ptr };
    let ctx = unsafe { Arc::from_raw(state.ctx_ptr) };
    let list_val = Value::from_bits(list_val_bits);
    let index_val = Value::from_bits(index_val_bits);
    let val = Value::from_bits(src_val_bits);
    let res = (|| {
        let index = index_val.as_number().map(|n| n as usize).ok_or_else(|| JitError::Runtime("List index must be a number".into(), line as usize, col as usize))?;
        if let Some(lid) = list_val.as_list_id() {
            let lists = ctx.lists.read().unwrap();
            if let Some(list_arc) = lists.get(lid as usize) {
                let mut list = list_arc.write().unwrap();
                if let Some(slot) = list.get_mut(index) { *slot = val; Ok(()) }
                else { Err(JitError::Runtime(format!("Index out of bounds: {}", index), line as usize, col as usize)) }
            } else { Err(JitError::Runtime("Invalid list ID".into(), line as usize, col as usize)) }
        } else { Err(JitError::Runtime("Expected list".into(), line as usize, col as usize)) }
    })();
    let _ = Arc::into_raw(ctx);
    if let Err(e) = res {
        state.error_occurred = true; state.error_line = line; state.error_col = col;
        state.error_msg = std::ffi::CString::new(e.to_string()).unwrap().into_raw();
    }
}

extern "C" fn rt_call_native(state_ptr: *mut RuntimeState, name_id: u32, args_ptr: *const u64, args_len: usize, line: u32, col: u32) -> u64 {
    let state = unsafe { &mut *state_ptr };
    let ctx = unsafe { Arc::from_raw(state.ctx_ptr) };
    let func_opt = ctx.native_fns.get(name_id as usize).cloned().flatten();
    
    let result = if let Some(func) = func_opt {
        let args_raw = unsafe { std::slice::from_raw_parts(args_ptr, args_len) };
        let args: Vec<Value> = args_raw.iter().map(|&bits| Value::from_bits(bits)).collect();
        let handle = Handle::current();
        let loc = Loc { line, col };
        match handle.block_on(func(ctx.clone(), args, loc)) {
            Ok(v) => v.to_bits(),
            Err(e) => {
                state.error_occurred = true; state.error_line = line; state.error_col = col;
                state.error_msg = std::ffi::CString::new(e.to_string()).unwrap().into_raw();
                0
            }
        }
    } else {
        state.error_occurred = true; state.error_line = line; state.error_col = col;
        state.error_msg = std::ffi::CString::new(format!("Unknown native function ID: {}", name_id)).unwrap().into_raw();
        0
    };
    let _ = Arc::into_raw(ctx);
    result
}

fn compile_and_run(program: Program) -> Result<(), JitError> {
    let builder = JITBuilder::new(cranelift_module::default_libcall_names()).map_err(|e| JitError::Runtime(format!("JIT builder error: {}", e), 0, 0))?;
    let mut module = JITModule::new(builder);
    let mut cranelift_ctx = module.make_context();
    let mut func_ctx = FunctionBuilderContext::new();
    let mut sig = module.make_signature();
    sig.params.push(AbiParam::new(types::I64)); sig.params.push(AbiParam::new(types::I64));
    let func_id = module.declare_function("main_jit", Linkage::Export, &sig).unwrap();
    cranelift_ctx.func.signature = sig;

    {
        let mut bc = FunctionBuilder::new(&mut cranelift_ctx.func, &mut func_ctx);
        let entry = bc.create_block(); bc.append_block_params_for_function_params(entry); bc.switch_to_block(entry);
        let regs_ptr = bc.block_params(entry)[0]; let state_ptr = bc.block_params(entry)[1];

        // Sig helpers
        let mut b_sig = module.make_signature(); b_sig.params.push(AbiParam::new(types::I64)); b_sig.params.push(AbiParam::new(types::I64)); b_sig.returns.push(AbiParam::new(types::I64));
        let fn_add = module.declare_function("rt_add", Linkage::Import, &b_sig).unwrap();
        let add_ref = module.declare_func_in_func(fn_add, bc.func);
        let fn_sub = module.declare_function("rt_sub", Linkage::Import, &b_sig).unwrap();
        let sub_ref = module.declare_func_in_func(fn_sub, bc.func);
        let fn_mul = module.declare_function("rt_mul", Linkage::Import, &b_sig).unwrap();
        let mul_ref = module.declare_func_in_func(fn_mul, bc.func);
        let fn_div = module.declare_function("rt_div", Linkage::Import, &b_sig).unwrap();
        let div_ref = module.declare_func_in_func(fn_div, bc.func);
        let fn_lt = module.declare_function("rt_less_than", Linkage::Import, &b_sig).unwrap();
        let lt_ref = module.declare_func_in_func(fn_lt, bc.func);

        let mut nl_sig = module.make_signature(); nl_sig.params.push(AbiParam::new(types::I64)); nl_sig.params.push(AbiParam::new(types::I64)); nl_sig.returns.push(AbiParam::new(types::I64));
        let fn_nl = module.declare_function("rt_new_list", Linkage::Import, &nl_sig).unwrap();
        let nl_ref = module.declare_func_in_func(fn_nl, bc.func);

        let mut lg_sig = module.make_signature(); lg_sig.params.push(AbiParam::new(types::I64)); lg_sig.params.push(AbiParam::new(types::I64)); lg_sig.params.push(AbiParam::new(types::I64)); lg_sig.params.push(AbiParam::new(types::I32)); lg_sig.params.push(AbiParam::new(types::I32)); lg_sig.returns.push(AbiParam::new(types::I64));
        let fn_lg = module.declare_function("rt_list_get", Linkage::Import, &lg_sig).unwrap();
        let lg_ref = module.declare_func_in_func(fn_lg, bc.func);

        let mut ls_sig = module.make_signature(); ls_sig.params.push(AbiParam::new(types::I64)); ls_sig.params.push(AbiParam::new(types::I64)); ls_sig.params.push(AbiParam::new(types::I64)); ls_sig.params.push(AbiParam::new(types::I64)); ls_sig.params.push(AbiParam::new(types::I32)); ls_sig.params.push(AbiParam::new(types::I32));
        let fn_ls = module.declare_function("rt_list_set", Linkage::Import, &ls_sig).unwrap();
        let ls_ref = module.declare_func_in_func(fn_ls, bc.func);

        let mut n_sig = module.make_signature(); n_sig.params.push(AbiParam::new(types::I64)); n_sig.params.push(AbiParam::new(types::I32)); n_sig.params.push(AbiParam::new(types::I64)); n_sig.params.push(AbiParam::new(types::I64)); n_sig.params.push(AbiParam::new(types::I32)); n_sig.params.push(AbiParam::new(types::I32)); n_sig.returns.push(AbiParam::new(types::I64));
        let fn_cn = module.declare_function("rt_call_native", Linkage::Import, &n_sig).unwrap();
        let cn_ref = module.declare_func_in_func(fn_cn, bc.func);

        let mut vars = Vec::new();
        for _ in 0..program.locals_count {
            let var = bc.declare_var(types::I64); // Matches 0.128 signature found by search
            vars.push(var);
        }
        for (i, var) in vars.iter().enumerate() {
            let offset = (i * 8) as i32;
            let val = bc.ins().load(types::I64, MemFlags::new(), regs_ptr, offset);
            bc.def_var(*var, val);
        }

        let mut blocks: Vec<Block> = program.instructions.iter().map(|_| bc.create_block()).collect();
        let exit = bc.create_block();
        bc.ins().jump(blocks[0], &[]);

        for (i, instr) in program.instructions.iter().enumerate() {
            bc.switch_to_block(blocks[i]);
            let next = if i + 1 < blocks.len() { blocks[i + 1] } else { exit };

            match instr {
                Instruction::LoadLiteral { dst, val } => {
                    let imm = bc.ins().iconst(types::I64, val.to_bits() as i64); bc.def_var(vars[*dst], imm); bc.ins().jump(next, &[]);
                }
                Instruction::Move { dst, src } => {
                    let v = bc.use_var(vars[*src]); bc.def_var(vars[*dst], v); bc.ins().jump(next, &[]);
                }
                Instruction::Add { dst, lhs, rhs, .. } => {
                    let l = bc.use_var(vars[*lhs]); let r = bc.use_var(vars[*rhs]); let res = bc.ins().call(add_ref, &[l, r]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::Sub { dst, lhs, rhs, .. } => {
                    let l = bc.use_var(vars[*lhs]); let r = bc.use_var(vars[*rhs]); let res = bc.ins().call(sub_ref, &[l, r]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::Mul { dst, lhs, rhs, .. } => {
                    let l = bc.use_var(vars[*lhs]); let r = bc.use_var(vars[*rhs]); let res = bc.ins().call(mul_ref, &[l, r]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::Div { dst, lhs, rhs, .. } => {
                    let l = bc.use_var(vars[*lhs]); let r = bc.use_var(vars[*rhs]); let res = bc.ins().call(div_ref, &[l, r]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::LessThan { dst, lhs, rhs, .. } => {
                    let l = bc.use_var(vars[*lhs]); let r = bc.use_var(vars[*rhs]); let res = bc.ins().call(lt_ref, &[l, r]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::Jump(target) => { bc.ins().jump(blocks[*target], &[]); }
                Instruction::JumpIfFalse { cond, target } => {
                    let val = bc.use_var(vars[*cond]);
                    let f_bits = Value::bool(false).to_bits() as i64;
                    let is_f = bc.ins().icmp_imm(IntCC::Equal, val, f_bits);
                    bc.ins().brif(is_f, blocks[*target], next);
                }
                Instruction::LoadGlobal { dst, global } => {
                    let g_ptr = bc.ins().load(types::I64, MemFlags::new(), state_ptr, 0);
                    let val = bc.ins().load(types::I64, MemFlags::new(), g_ptr, (*global * 8) as i32); bc.def_var(vars[*dst], val); bc.ins().jump(next, &[]);
                }
                Instruction::StoreGlobal { global, src } => {
                    let v = bc.use_var(vars[*src]);
                    let g_ptr = bc.ins().load(types::I64, MemFlags::new(), state_ptr, 0);
                    bc.ins().store(MemFlags::new(), v, g_ptr, (*global * 8) as i32); bc.ins().jump(next, &[]);
                }
                Instruction::NewList { dst, len } => {
                    let res = bc.ins().call(nl_ref, &[state_ptr, bc.ins().iconst(types::I64, *len as i64)]); bc.def_var(vars[*dst], bc.inst_results(res)[0]); bc.ins().jump(next, &[]);
                }
                Instruction::ListGet { dst, list, index_reg, loc } => {
                    let l = bc.use_var(vars[*list]); let iv = bc.use_var(vars[*index_reg]);
                    let res = bc.ins().call(lg_ref, &[state_ptr, l, iv, bc.ins().iconst(types::I32, loc.line as i64), bc.ins().iconst(types::I32, loc.col as i64)]); bc.def_var(vars[*dst], bc.inst_results(res)[0]);
                    let err = bc.ins().load(types::I8, MemFlags::new(), state_ptr, 16); let has_err = bc.ins().icmp_imm(IntCC::NotEqual, err, 0); bc.ins().brif(has_err, exit, next);
                }
                Instruction::ListSet { list, index_reg, src, loc } => {
                    let l = bc.use_var(vars[*list]); let iv = bc.use_var(vars[*index_reg]); let s = bc.use_var(vars[*src]);
                    bc.ins().call(ls_ref, &[state_ptr, l, iv, s, bc.ins().iconst(types::I32, loc.line as i64), bc.ins().iconst(types::I32, loc.col as i64)]);
                    let err = bc.ins().load(types::I8, MemFlags::new(), state_ptr, 16); let has_err = bc.ins().icmp_imm(IntCC::NotEqual, err, 0); bc.ins().brif(has_err, exit, next);
                }
                Instruction::CallNative { name_id, args_regs, dst, loc } => {
                    let slot = bc.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, (args_regs.len() * 8) as u32));
                    for (arg_i, &reg) in args_regs.iter().enumerate() { let v = bc.use_var(vars[reg]); bc.ins().stack_store(v, slot, (arg_i * 8) as i32); }
                    let args_p = bc.ins().stack_addr(types::I64, slot, 0);
                    let res = bc.ins().call(cn_ref, &[state_ptr, bc.ins().iconst(types::I32, *name_id as i64), args_p, bc.ins().iconst(types::I64, args_regs.len() as i64), bc.ins().iconst(types::I32, loc.line as i64), bc.ins().iconst(types::I32, loc.col as i64)]);
                    if let Some(d) = dst { bc.def_var(vars[*d], bc.inst_results(res)[0]); }
                    let err = bc.ins().load(types::I8, MemFlags::new(), state_ptr, 16); let has_err = bc.ins().icmp_imm(IntCC::NotEqual, err, 0); bc.ins().brif(has_err, exit, next);
                }
                _ => { bc.ins().jump(next, &[]); }
            }
        }
        bc.switch_to_block(exit);
        for (i, var) in vars.iter().enumerate() { let v = bc.use_var(*var); bc.ins().store(MemFlags::new(), v, regs_ptr, (i * 8) as i32); }
        bc.ins().return_(&[]); bc.seal_all_blocks(); bc.finalize();
    }

    module.define_function(func_id, &mut cranelift_ctx).unwrap(); module.clear_context(&mut cranelift_ctx); module.finalize_definitions().unwrap();
    let code = module.get_finalized_function(func_id); let main_jit: extern "C" fn(*mut u64, *mut RuntimeState) = unsafe { std::mem::transmute(code) };
    let mut registers = vec![Value::from_bits(0); program.locals_count]; let mut globals = vec![0u64; program.globals_count];
    let mut registry = rustc_hash::FxHashMap::default(); setup_native_fns(&mut registry);
    let mut native_fns = vec![None; program.string_pool.len()]; for (name, func) in registry { if let Some(id) = program.string_pool.iter().position(|s| s.as_ref() == name) { native_fns[id] = Some(func); } }
    let ctx_arc = Arc::new(Context { globals: Vec::new(), string_pool: program.string_pool.clone(), lists: std::sync::RwLock::new(Vec::new()), native_fns });
    let mut state = RuntimeState { globals_ptr: globals.as_mut_ptr(), ctx_ptr: Arc::into_raw(ctx_arc.clone()), error_occurred: false, error_msg: std::ptr::null_mut(), error_line: 0, error_col: 0 };
    main_jit(registers.as_mut_ptr() as *mut u64, &mut state);
    let _ = unsafe { Arc::from_raw(state.ctx_ptr) };
    if state.error_occurred { let msg = unsafe { std::ffi::CString::from_raw(state.error_msg).into_string().unwrap() }; return Err(JitError::Runtime(msg, state.error_line as usize, state.error_col as usize)); }
    Ok(())
}
