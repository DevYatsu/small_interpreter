//! Execution backends and runtime structures.
//!
//! This module defines the [`Backend`] trait, which abstracts over different
//! runtime execution strategies (e.g. tree-walk, bytecode virtual machine, or
//! in the future, native compilation).  Currently, the only implementation is
//! [`interpreter::Interpreter`], a fast, register-based async bytecode VM.
//!
//! # Memory management
//!
//! Unboxed values (numbers, booleans, SSO strings) are passed directly in registers,
//! while complex items (lists, objects, long strings) are allocated into the
//! [`Heap`].  The heap features:
//! - Thread-safe `AtomicU64` slots for lock-free parallel data races
//! - A nursery and tenured generation with write barriers ([`Generation`])
//! - A parallel mark-and-sweep garbage collector

use crate::compiler::{Loc, Program, Value};
use crate::error::JitError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

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
    List(parking_lot::RwLock<Vec<AtomicU64>>),
    Object(parking_lot::RwLock<rustc_hash::FxHashMap<u32, AtomicU64>>),
    Timestamp(std::time::Instant),
    Range { start: f64, end: f64, step: f64 },
    BoundMethod { receiver: Value, name_id: u32 },
}

impl ManagedObject {
    pub fn visit_children<F>(&self, mut f: F)
    where
        F: FnMut(u32),
    {
        match self {
            ManagedObject::List(elements) => {
                let elements = elements.read();
                for atomic_v in elements.iter() {
                    let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                    if let Some(child_id) = v.as_obj_id() {
                        f(child_id);
                    }
                }
            }
            ManagedObject::Object(fields) => {
                let fields = fields.read();
                for atomic_v in fields.values() {
                    let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                    if let Some(child_id) = v.as_obj_id() {
                        f(child_id);
                    }
                }
            }
            ManagedObject::String(_) => {}
            ManagedObject::Timestamp(_) => {}
            ManagedObject::Range { .. } => {}
            ManagedObject::BoundMethod { receiver, .. } => {
                if let Some(child_id) = receiver.as_obj_id() {
                    f(child_id);
                }
            }
        }
    }
}

pub struct HeapObject {
    pub obj: ManagedObject,
    pub last_gc_id: u32,
    pub generation: Generation,
}

#[derive(Clone)]
pub enum Callable {
    User(crate::compiler::UserFunction),
    Native(NativeFn),
}

pub type TaskRegisters = Arc<Mutex<Vec<Arc<[AtomicU64]>>>>;

pub struct Context {
    pub globals: Arc<[AtomicU64]>,
    pub string_pool: Arc<[Arc<str>]>,
    pub callables: Arc<[Option<Callable>]>,
    pub active_registers: Mutex<Vec<TaskRegisters>>,
    pub heap: Heap,
}

pub struct Heap {
    pub objects: parking_lot::RwLock<Vec<Option<HeapObject>>>,
    pub metadata: Mutex<HeapMetadata>,
    pub gc_count: AtomicU32,
    pub alloc_since_gc: AtomicUsize,
}

use rayon::prelude::*;

impl Heap {
    pub fn collect_garbage(&self, ctx: &Context) {
        let gc_id = self.gc_count.fetch_add(1, Ordering::Relaxed) + 1;
        if gc_id.is_multiple_of(5) {
            self.major_gc(gc_id, ctx);
        } else {
            self.minor_gc(gc_id, ctx);
        }
    }

    pub fn major_gc(&self, gc_id: u32, ctx: &Context) {
        let mut objects = self.objects.write();
        let mut worklist = Vec::new();

        self.trace_roots(ctx, &mut worklist);

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                obj.obj.visit_children(|child_id| worklist.push(child_id));
            }
        }

        let mut meta = self.metadata.lock().unwrap();
        meta.remembered_set.clear();
        meta.nursery_ids.clear();

        let free_ids: Vec<u32> = objects
            .par_iter_mut()
            .enumerate()
            .filter_map(|(i, slot)| {
                if let Some(obj) = slot {
                    if obj.last_gc_id != gc_id {
                        *slot = None;
                        return Some(i as u32);
                    } else {
                        obj.generation = Generation::Tenured;
                    }
                }
                None
            })
            .collect();

        meta.free_list.extend(free_ids);
    }

    pub fn minor_gc(&self, gc_id: u32, ctx: &Context) {
        let mut objects = self.objects.write();
        let mut worklist = Vec::new();

        self.trace_roots(ctx, &mut worklist);
        {
            let meta = self.metadata.lock().unwrap();
            worklist.extend(meta.remembered_set.iter());
        }

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                obj.obj.visit_children(|child_id| worklist.push(child_id));
            }
        }

        let mut meta = self.metadata.lock().unwrap();
        let mut promoted_ids = Vec::new();

        let ids: Vec<u32> = meta.nursery_ids.drain(..).collect();
        for id in ids {
            if let Some(Some(obj)) = objects.get_mut(id as usize) {
                if obj.last_gc_id != gc_id {
                    objects[id as usize] = None;
                    meta.free_list.push(id);
                } else {
                    obj.generation = Generation::Tenured;
                    promoted_ids.push(id);
                }
            }
        }

        let remembered_set = &meta.remembered_set;
        let new_remembered_from_old: Vec<u32> = remembered_set
            .par_iter()
            .filter(|&&id| {
                if let Some(Some(obj)) = objects.get(id as usize)
                    && obj.generation == Generation::Tenured
                    && self.check_points_to_nursery(obj, &objects)
                {
                    true
                } else {
                    false
                }
            })
            .copied()
            .collect();

        let new_remembered_from_promoted: Vec<u32> = promoted_ids
            .into_par_iter()
            .filter(|&id| {
                if let Some(Some(obj)) = objects.get(id as usize)
                    && self.check_points_to_nursery(obj, &objects)
                {
                    true
                } else {
                    false
                }
            })
            .collect();

        let mut new_set = rustc_hash::FxHashSet::default();
        new_set.extend(new_remembered_from_old);
        new_set.extend(new_remembered_from_promoted);
        meta.remembered_set = new_set;
    }

    fn trace_roots(&self, ctx: &Context, worklist: &mut Vec<u32>) {
        worklist.extend(0..ctx.string_pool.len() as u32);
        for global in ctx.globals.iter() {
            if let Some(id) = Value::from_bits(global.load(Ordering::Relaxed)).as_obj_id() {
                worklist.push(id);
            }
        }
        let active_tasks = ctx.active_registers.lock().unwrap();
        for task_roots in active_tasks.iter() {
            let regs_stack = task_roots.lock().unwrap();
            for regs in regs_stack.iter() {
                for atomic_val in regs.iter() {
                    if let Some(id) =
                        Value::from_bits(atomic_val.load(Ordering::Relaxed)).as_obj_id()
                    {
                        worklist.push(id);
                    }
                }
            }
        }
    }

    pub fn check_points_to_nursery(
        &self,
        obj: &HeapObject,
        heap_objs: &[Option<HeapObject>],
    ) -> bool {
        let mut found = false;
        obj.obj.visit_children(|child_id| {
            if !found
                && let Some(Some(child)) = heap_objs.get(child_id as usize)
                && child.generation == Generation::Nursery
            {
                found = true;
            }
        });
        found
    }
}

pub struct HeapMetadata {
    pub free_list: Vec<u32>,
    pub nursery_ids: Vec<u32>,
    pub remembered_set: rustc_hash::FxHashSet<u32>,
}

impl Context {
    #[inline]
    pub fn get_callable(&self, name_id: u32) -> Option<&Callable> {
        unsafe { self.callables.get_unchecked(name_id as usize).as_ref() }
    }

    #[inline]
    pub fn value_as_pool_id(&self, val: Value) -> Option<u32> {
        let bits = val.to_bits();
        let tag = (bits & crate::compiler::TAG_MASK) >> 48;
        if (3..=9).contains(&tag) {
            let len = (tag - 3) as usize;
            let mut bytes = [0u8; 6];
            for (i, byte) in bytes.iter_mut().enumerate().take(len) {
                *byte = ((bits >> (i * 8)) & 0xFF) as u8;
            }
            let s = std::str::from_utf8(&bytes[..len]).ok()?;
            self.string_pool
                .iter()
                .position(|p| p.as_ref() == s)
                .map(|i| i as u32)
        } else if let Some(oid) = val.as_obj_id() {
            if (oid as usize) < self.string_pool.len() {
                return Some(oid);
            }
            let heap = self.heap.objects.read();
            if let Some(Some(obj)) = heap.get(oid as usize)
                && let ManagedObject::String(s) = &obj.obj
            {
                return self
                    .string_pool
                    .iter()
                    .position(|p| p.as_ref() == s.as_ref())
                    .map(|i| i as u32);
            }
            None
        } else {
            None
        }
    }

    pub fn alloc(&self, obj: ManagedObject, dst: &AtomicU64) -> u32 {
        let count = self.heap.alloc_since_gc.fetch_add(1, Ordering::Relaxed);
        if count >= 100000
            && self
                .heap
                .alloc_since_gc
                .compare_exchange(count + 1, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.heap.collect_garbage(self);
        }

        let mut objects = self.heap.objects.write();
        let mut meta = self.heap.metadata.lock().unwrap();

        let id = if let Some(id) = meta.free_list.pop() {
            objects[id as usize] = Some(HeapObject {
                obj,
                last_gc_id: 0,
                generation: Generation::Nursery,
            });
            id
        } else {
            let id = objects.len() as u32;
            objects.push(Some(HeapObject {
                obj,
                last_gc_id: 0,
                generation: Generation::Nursery,
            }));
            id
        };

        dst.store(Value::object(id).to_bits(), Ordering::Relaxed);
        meta.nursery_ids.push(id);
        id
    }

    pub fn values_equal(&self, v1: Value, v2: Value) -> bool {
        let b1 = v1.to_bits();
        let b2 = v2.to_bits();
        if b1 == b2 {
            return true;
        }
        if let (Some(n1), Some(n2)) = (v1.as_number(), v2.as_number()) {
            return n1 == n2;
        }

        let tag1 = (b1 & crate::compiler::TAG_MASK) >> 48;
        let tag2 = (b2 & crate::compiler::TAG_MASK) >> 48;
        if (3..=9).contains(&tag1) || (3..=9).contains(&tag2) {
            let s1 = v1.as_string(self);
            let s2 = v2.as_string(self);
            return s1 == s2;
        }

        if let (Some(id1), Some(id2)) = (v1.as_obj_id(), v2.as_obj_id()) {
            let heap = self.heap.objects.read();
            if id1 < heap.len() as u32
                && id2 < heap.len() as u32
                && let (Some(o1), Some(o2)) = (&heap[id1 as usize], &heap[id2 as usize])
                && let (ManagedObject::String(s1), ManagedObject::String(s2)) = (&o1.obj, &o2.obj)
            {
                return s1 == s2;
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
