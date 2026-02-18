use crate::compiler::{Loc, Program, Value};
use crate::error::JitError;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

pub mod cranelift;
pub mod interpreter;

pub trait Backend {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>>;
}

/// Shared Virtul Machine State.
pub struct Context {
    /// Atomic global variables (NaN-Boxed bits).
    pub globals: Vec<AtomicU64>,
    /// Shared string pool for ID-lookup.
    pub string_pool: Arc<[Arc<str>]>,
    /// Global list pool.
    pub lists: std::sync::RwLock<Vec<Arc<std::sync::RwLock<Vec<Value>>>>>,
    /// Optimized native function lookup (maps name_id -> function).
    pub native_fns: Vec<Option<NativeFn>>,
}

pub type NativeFn = Arc<
    dyn Fn(
            Arc<Context>,
            Vec<Value>,
            Loc,
        ) -> Pin<Box<dyn Future<Output = Result<Value, JitError>> + Send>>
        + Send
        + Sync,
>;

pub fn setup_native_fns(fns: &mut rustc_hash::FxHashMap<String, NativeFn>) {
    fns.insert(
        "print".to_string(),
        Arc::new(|ctx, args, _| {
            Box::pin(async move {
                let mut stdout = std::io::stdout().lock();
                for (i, val) in args.iter().enumerate() {
                    if i > 0 {
                        let _ = write!(stdout, " ");
                    }
                    print_value(&mut stdout, &ctx, *val);
                }
                let _ = writeln!(stdout);
                let _ = stdout.flush();
                Ok(Value::from_bits(0))
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
                if let Some(sid) = val.as_string_id() {
                    let url = &ctx.string_pool[sid as usize];
                    match reqwest::get(url.as_ref()).await {
                        Ok(resp) => {
                            let status = resp.status();
                            let body = resp
                                .text()
                                .await
                                .unwrap_or_else(|_| "Error reading body".to_string());
                            let mut stdout = std::io::stdout().lock();
                            let _ = writeln!(stdout, "Fetch {}: {} - {}", url, status, body);
                            let _ = stdout.flush();
                            Ok(Value::from_bits(0))
                        }
                        Err(e) => {
                            let mut stdout = std::io::stdout().lock();
                            let _ = writeln!(stdout, "Fetch {} failed: {}", url, e);
                            let _ = stdout.flush();
                            Ok(Value::from_bits(0))
                        }
                    }
                } else {
                    Err(JitError::Runtime(
                        "fetch error: expected string URL".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ))
                }
            })
        }),
    );
}

fn print_value(stdout: &mut std::io::StdoutLock, ctx: &Context, val: Value) {
    if let Some(n) = val.as_number() {
        let _ = write!(stdout, "{}", n);
    } else if let Some(b) = val.as_bool() {
        let _ = write!(stdout, "{}", b);
    } else if let Some(sid) = val.as_string_id() {
        let s = &ctx.string_pool[sid as usize];
        let _ = write!(stdout, "{}", s);
    } else if let Some(lid) = val.as_list_id() {
        let lists = ctx.lists.read().unwrap();
        if let Some(list_arc) = lists.get(lid as usize) {
            let list = list_arc.read().unwrap();
            let _ = write!(stdout, "[");
            for (i, v) in list.iter().enumerate() {
                if i > 0 {
                    let _ = write!(stdout, ", ");
                }
                print_value_nested(stdout, ctx, *v);
            }
            let _ = write!(stdout, "]");
        }
    }
}

fn print_value_nested(stdout: &mut std::io::StdoutLock, ctx: &Context, val: Value) {
    if let Some(sid) = val.as_string_id() {
        let s = &ctx.string_pool[sid as usize];
        let _ = write!(stdout, "\"{}\"", s);
    } else if val.as_list_id().is_some() {
        let _ = write!(stdout, "[...]");
    } else {
        print_value(stdout, ctx, val);
    }
}
