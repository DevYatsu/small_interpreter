use crate::compiler::{Loc, Program, Value};
use crate::error::JitError;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
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
    pub last_gc_id: u32,
    pub generation: Generation,
}

#[derive(Clone)]
pub enum Callable {
    User(crate::compiler::UserFunction),
    Native(NativeFn),
}

pub struct Context {
    pub globals: Arc<[AtomicU64]>,
    pub string_pool: Arc<[Arc<str>]>,
    pub callables: Arc<[Option<Callable>]>,
    pub active_registers: Mutex<Vec<Arc<[AtomicU64]>>>,
    pub heap: Heap,
}

pub struct Heap {
    pub objects: RwLock<Vec<Option<HeapObject>>>,
    pub metadata: Mutex<HeapMetadata>,
    pub gc_count: AtomicU32,
    pub alloc_since_gc: AtomicUsize,
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
            for i in 0..len {
                bytes[i] = ((bits >> (i * 8)) & 0xFF) as u8;
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
            let heap = self.heap.objects.read().unwrap();
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
        if count >= 10000
            && self
                .heap
                .alloc_since_gc
                .compare_exchange(count + 1, 0, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
        {
            self.collect_garbage();
        }

        let mut objects = self.heap.objects.write().unwrap();
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

    pub fn collect_garbage(&self) {
        let gc_id = self.heap.gc_count.fetch_add(1, Ordering::Relaxed) + 1;
        if gc_id % 5 == 0 {
            self.major_gc(gc_id);
        } else {
            self.minor_gc(gc_id);
        }
    }

    pub fn major_gc(&self, gc_id: u32) {
        let mut objects = self.heap.objects.write().unwrap();
        let mut worklist = Vec::new();

        self.trace_roots(&objects, &mut worklist);

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                self.trace_object_ids(obj, &mut worklist);
            }
        }

        let mut meta = self.heap.metadata.lock().unwrap();
        meta.remembered_set.clear();
        meta.nursery_ids.clear();

        for i in 0..objects.len() {
            if let Some(ref mut obj) = objects[i] {
                if obj.last_gc_id != gc_id {
                    objects[i] = None;
                    meta.free_list.push(i as u32);
                } else {
                    obj.generation = Generation::Tenured;
                }
            }
        }
    }

    pub fn minor_gc(&self, gc_id: u32) {
        let mut objects = self.heap.objects.write().unwrap();
        let mut worklist = Vec::new();

        self.trace_roots(&objects, &mut worklist);
        {
            let meta = self.heap.metadata.lock().unwrap();
            for &id in meta.remembered_set.iter() {
                worklist.push(id);
            }
        }

        while let Some(id) = worklist.pop() {
            if let Some(Some(obj)) = objects.get_mut(id as usize)
                && obj.last_gc_id != gc_id
            {
                obj.last_gc_id = gc_id;
                self.trace_object_ids(obj, &mut worklist);
            }
        }

        let mut meta = self.heap.metadata.lock().unwrap();
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

        let mut new_remembered = rustc_hash::FxHashSet::default();
        for &id in meta.remembered_set.iter() {
            if let Some(Some(obj)) = objects.get(id as usize)
                && obj.generation == Generation::Tenured
                && self.check_points_to_nursery(obj, &objects)
            {
                new_remembered.insert(id);
            }
        }

        for id in promoted_ids {
            if let Some(Some(obj)) = objects.get(id as usize)
                && self.check_points_to_nursery(obj, &objects)
            {
                new_remembered.insert(id);
            }
        }
        meta.remembered_set = new_remembered;
    }

    fn trace_roots(&self, _objects: &[Option<HeapObject>], worklist: &mut Vec<u32>) {
        for i in 0..self.string_pool.len() {
            worklist.push(i as u32);
        }
        for global in self.globals.iter() {
            let val = Value::from_bits(global.load(Ordering::Relaxed));
            if let Some(id) = val.as_obj_id() {
                worklist.push(id);
            }
        }
        let active_regs = self.active_registers.lock().unwrap();
        for regs in active_regs.iter() {
            for atomic_val in regs.iter() {
                let val = Value::from_bits(atomic_val.load(Ordering::Relaxed));
                if let Some(id) = val.as_obj_id() {
                    worklist.push(id);
                }
            }
        }
    }

    fn trace_object_ids(&self, obj: &HeapObject, worklist: &mut Vec<u32>) {
        if let ManagedObject::List(elements) = &obj.obj {
            for atomic_v in elements.iter() {
                let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                if let Some(child_id) = v.as_obj_id() {
                    worklist.push(child_id);
                }
            }
        }
    }

    pub fn check_points_to_nursery(&self, obj: &HeapObject, heap: &[Option<HeapObject>]) -> bool {
        if let ManagedObject::List(elements) = &obj.obj {
            for atomic_v in elements.iter() {
                let v = Value::from_bits(atomic_v.load(Ordering::Relaxed));
                if let Some(child_id) = v.as_obj_id()
                    && let Some(Some(child)) = heap.get(child_id as usize)
                    && child.generation == Generation::Nursery
                {
                    return true;
                }
            }
        }
        false
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
            let heap = self.heap.objects.read().unwrap();
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
