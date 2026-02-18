use crate::compiler::{Loc, Program, Value};
use crate::error::JitError;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub mod interpreter;

pub trait Backend {
    fn run(&self, program: Program) -> Pin<Box<dyn Future<Output = Result<(), JitError>> + Send>>;
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Generation {
    Nursery,
    Tenured,
}

pub enum ManagedObject {
    String(Arc<str>),
    List(Box<[AtomicU64]>),
}

pub struct HeapObject {
    pub obj: ManagedObject,
    pub marked: bool,
    pub generation: Generation,
}

pub struct Context {
    pub globals: Vec<AtomicU64>,
    pub string_pool: Arc<[Arc<str>]>,
    pub heap: RwLock<Vec<Option<HeapObject>>>,
    pub free_list: Mutex<Vec<u32>>,
    pub native_fns: Vec<Option<NativeFn>>,
    pub active_registers: RwLock<Vec<Arc<[AtomicU64]>>>,
    pub remembered_set: Mutex<rustc_hash::FxHashSet<u32>>,
    pub gc_count: Mutex<u32>,
    pub functions: Arc<[crate::compiler::UserFunction]>,
}

impl Context {
    pub fn alloc(&self, obj: ManagedObject) -> u32 {
        {
            let heap_len = self.heap.read().unwrap().len();
            if heap_len > 1000 && heap_len % 500 == 0 {
                self.collect_garbage();
            }
        }

        let mut free_list = self.free_list.lock().unwrap();
        if let Some(id) = free_list.pop() {
            let mut heap = self.heap.write().unwrap();
            heap[id as usize] = Some(HeapObject {
                obj,
                marked: false,
                generation: Generation::Nursery,
            });
            id
        } else {
            let mut heap = self.heap.write().unwrap();
            let id = heap.len() as u32;
            heap.push(Some(HeapObject {
                obj,
                marked: false,
                generation: Generation::Nursery,
            }));
            id
        }
    }

    pub fn collect_garbage(&self) {
        let mut gc_count = self.gc_count.lock().unwrap();
        *gc_count += 1;

        if *gc_count % 5 == 0 {
            self.major_gc();
        } else {
            self.minor_gc();
        }
    }

    pub fn major_gc(&self) {
        let mut heap = self.heap.write().unwrap();
        let mut worklist = Vec::new();

        self.trace_roots(&mut worklist);

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = heap.get_mut(id as usize) {
                if !obj.marked {
                    obj.marked = true;
                    self.trace_object(obj, &mut worklist);
                }
            }
        }

        let mut free_list = self.free_list.lock().unwrap();
        let mut remembered_set = self.remembered_set.lock().unwrap();
        remembered_set.clear();

        for i in 0..heap.len() {
            if let Some(ref mut obj) = heap[i] {
                if !obj.marked {
                    heap[i] = None;
                    free_list.push(i as u32);
                } else {
                    obj.marked = false;
                    obj.generation = Generation::Tenured;
                }
            }
        }
    }

    pub fn minor_gc(&self) {
        let mut heap = self.heap.write().unwrap();
        let mut worklist = Vec::new();

        self.trace_roots(&mut worklist);
        {
            let remembered = self.remembered_set.lock().unwrap();
            for &id in remembered.iter() {
                worklist.push(id);
            }
        }

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = heap.get_mut(id as usize) {
                if !obj.marked {
                    obj.marked = true;
                    self.trace_object(obj, &mut worklist);
                }
            }
        }

        let mut free_list = self.free_list.lock().unwrap();
        let mut new_remembered = rustc_hash::FxHashSet::default();

        for i in 0..heap.len() {
            let (is_nursery, is_marked) = match &heap[i] {
                Some(obj) => (obj.generation == Generation::Nursery, obj.marked),
                None => continue,
            };

            if is_nursery {
                if !is_marked {
                    heap[i] = None;
                    free_list.push(i as u32);
                } else {
                    let obj = heap[i].as_mut().unwrap();
                    obj.marked = false;
                    obj.generation = Generation::Tenured;
                }
            } else {
                let obj = heap[i].as_mut().unwrap();
                obj.marked = false;
            }
        }

        // Final pass for remembered set (entire heap borrowed immutably)
        for i in 0..heap.len() {
            if let Some(obj) = &heap[i] {
                if obj.generation == Generation::Tenured && self.check_points_to_nursery(obj, &heap)
                {
                    new_remembered.insert(i as u32);
                }
            }
        }

        let mut remembered_set = self.remembered_set.lock().unwrap();
        *remembered_set = new_remembered;
    }

    fn trace_roots(&self, worklist: &mut Vec<u32>) {
        for global in &self.globals {
            let val = Value::from_bits(global.load(Ordering::Relaxed));
            if let Some(id) = val.as_obj_id() {
                worklist.push(id);
            }
        }

        let active_regs = self.active_registers.read().unwrap();
        for regs in active_regs.iter() {
            for atomic_val in regs.iter() {
                let val = Value::from_bits(atomic_val.load(Ordering::Relaxed));
                if let Some(id) = val.as_obj_id() {
                    worklist.push(id);
                }
            }
        }
    }

    fn trace_object(&self, obj: &HeapObject, worklist: &mut Vec<u32>) {
        if let ManagedObject::List(elements) = &obj.obj {
            for atomic_v in elements.iter() {
                let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                if let Some(child_id) = v.as_obj_id() {
                    worklist.push(child_id);
                }
            }
        }
    }

    fn check_points_to_nursery(&self, obj: &HeapObject, heap: &[Option<HeapObject>]) -> bool {
        if let ManagedObject::List(elements) = &obj.obj {
            for atomic_v in elements.iter() {
                let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                if let Some(child_id) = v.as_obj_id() {
                    if let Some(Some(child)) = heap.get(child_id as usize) {
                        if child.generation == Generation::Nursery {
                            return true;
                        }
                    }
                }
            }
        }
        false
    }
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
                let url = if let Some(s) = val.as_sso() {
                    Arc::from(s)
                } else if let Some(oid) = val.as_obj_id() {
                    let heap = ctx.heap.read().unwrap();
                    if let Some(Some(HeapObject {
                        obj: ManagedObject::String(s),
                        ..
                    })) = heap.get(oid as usize)
                    {
                        s.clone()
                    } else {
                        return Err(JitError::Runtime(
                            "fetch error: expected string URL".into(),
                            loc.line as usize,
                            loc.col as usize,
                        ));
                    }
                } else {
                    return Err(JitError::Runtime(
                        "fetch error: expected string URL".into(),
                        loc.line as usize,
                        loc.col as usize,
                    ));
                };

                match reqwest::get(url.as_ref()).await {
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
}

fn print_value(ctx: &Context, val: Value) {
    if let Some(n) = val.as_number() {
        print!("{}", n);
    } else if let Some(b) = val.as_bool() {
        print!("{}", b);
    } else if let Some(s) = val.as_sso() {
        print!("{}", s);
    } else if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.read().unwrap();
        if let Some(Some(HeapObject { obj, .. })) = heap.get(oid as usize) {
            match obj {
                ManagedObject::String(s) => {
                    print!("{}", s);
                }
                ManagedObject::List(elements) => {
                    print!("[");
                    for (i, atomic_v) in elements.iter().enumerate() {
                        if i > 0 {
                            print!(", ");
                        }
                        let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                        print_value_nested(ctx, v);
                    }
                    print!("]");
                }
            }
        }
    }
}

fn print_value_nested(ctx: &Context, val: Value) {
    if let Some(s) = val.as_sso() {
        print!("\"{}\"", s);
    } else if let Some(oid) = val.as_obj_id() {
        let heap = ctx.heap.read().unwrap();
        if let Some(Some(HeapObject { obj, .. })) = heap.get(oid as usize) {
            match obj {
                ManagedObject::String(s) => {
                    print!("\"{}\"", s);
                }
                ManagedObject::List(_) => {
                    print!("[...]");
                }
            }
        }
    } else {
        print_value(ctx, val);
    }
}
