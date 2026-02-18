use std::sync::Arc;

use crate::backends::{Context, ManagedObject};

/// Represents a location in the source code.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loc {
    pub line: u32,
    pub col: u32,
}

impl From<(usize, usize)> for Loc {
    fn from((line, col): (usize, usize)) -> Self {
        Self {
            line: line as u32,
            col: col as u32,
        }
    }
}

/// A "NaN-Boxed" Value.
///
/// We use the 64-bit space of a double-precision float to store all types.
/// - If the exponent is not all 1s, it's a valid f64 number.
/// - If it's a "Quiet NaN", we use the remaining payload bits to tag and store other types:
///   - Bool: Tagged with TAG_BOOL
///   - Object: Tagged with TAG_OBJ (contains an ID to a HeapObject)
///   - SSO (Small String Optimization): Integrated within the NaN payload for strings <= 6 bytes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Value(u64);

pub const QNAN: u64 = 0x7ff0000000000000;
pub const TAG_MASK: u64 = 0x000F000000000000;
pub const TAG_BOOL: u64 = 0x0001000000000000;
pub const TAG_OBJ: u64 = 0x0002000000000000;

impl Value {
    #[inline(always)]
    pub fn number(n: f64) -> Self {
        Self(n.to_bits())
    }

    #[inline(always)]
    pub fn bool(b: bool) -> Self {
        Self(QNAN | TAG_BOOL | (b as u64))
    }

    #[inline(always)]
    pub fn object(id: u32) -> Self {
        Self(QNAN | TAG_OBJ | (id as u64))
    }

    pub fn sso(s: &str) -> Option<Self> {
        if s.len() > 6 {
            return None;
        }

        // Tag is 3 + length.
        let bits = QNAN | ((3 + s.len() as u64) << 48);

        let mut payload: u64 = 0;
        for (i, byte) in s.as_bytes().iter().enumerate() {
            payload |= (*byte as u64) << (i * 8);
        }
        Some(Self(bits | payload))
    }

    #[inline(always)]
    pub fn as_number(self) -> Option<f64> {
        if (self.0 & QNAN) != QNAN {
            Some(f64::from_bits(self.0))
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_bool(self) -> Option<bool> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_BOOL) {
            Some((self.0 & 1) != 0)
        } else {
            None
        }
    }

    #[inline(always)]
    pub fn as_obj_id(self) -> Option<u32> {
        if (self.0 & (QNAN | TAG_MASK)) == (QNAN | TAG_OBJ) {
            Some((self.0 & 0xFFFFFFFF) as u32)
        } else {
            None
        }
    }

    pub fn with_str<R>(&self, ctx: &Context, f: impl FnOnce(&str) -> R) -> Option<R> {
        let bits = self.0;
        let tag = (bits & TAG_MASK) >> 48;
        if (3..=9).contains(&tag) {
            let len = (tag - 3) as usize;
            let mut bytes = [0u8; 6];
            for i in 0..len {
                bytes[i] = ((bits >> (i * 8)) & 0xFF) as u8;
            }
            let s = std::str::from_utf8(&bytes[..len]).ok()?;
            return Some(f(s));
        }
        if let Some(oid) = self.as_obj_id() {
            let heap = ctx.heap.objects.read().unwrap();
            if let Some(Some(obj)) = heap.get(oid as usize)
                && let ManagedObject::String(s) = &obj.obj
            {
                return Some(f(s.as_ref()));
            }
        }
        None
    }

    pub fn as_string(&self, ctx: &Context) -> Option<String> {
        self.with_str(ctx, |s| s.to_string())
    }

    #[inline(always)]
    pub fn to_bits(self) -> u64 {
        self.0
    }

    #[inline(always)]
    pub fn from_bits(bits: u64) -> Self {
        Self(bits)
    }
}

/// The instruction set for the interpreter.
#[derive(Debug, Clone, PartialEq)]
pub enum Instruction {
    /// Load a constant Value into a destination register.
    LoadLiteral { dst: usize, val: Value },
    /// Copy a value from one register to another.
    Move { dst: usize, src: usize },
    /// Load a value from a global variable into a register.
    LoadGlobal { dst: usize, global: usize },
    /// Store a value from a register into a global variable.
    StoreGlobal { global: usize, src: usize },
    /// Unconditional jump to a target instruction index.
    Jump(usize),
    /// Jump to a target index if the condition register evaluates to false.
    JumpIfFalse { cond: usize, target: usize },
    /// Add numbers or concatenate strings.
    Add {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Subtract two numbers.
    Sub {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Multiply two numbers.
    Mul {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Divide two numbers.
    Div {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Atomic increment of a local register (expected to contain a number).
    Increment(usize),
    /// Atomic increment of a global variable (expected to contain a number).
    IncrementGlobal(usize),
    /// Compare two values for equality.
    Eq { dst: usize, lhs: usize, rhs: usize },
    /// Compare two values for inequality.
    Ne { dst: usize, lhs: usize, rhs: usize },
    /// Less than comparison.
    Lt {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Less than or equal comparison.
    Le {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Greater than comparison.
    Gt {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Greater than or equal comparison.
    Ge {
        dst: usize,
        lhs: usize,
        rhs: usize,
        loc: Loc,
    },
    /// Spawn a new concurrent task.
    Spawn {
        /// The instructions for the new task.
        instructions: Arc<[Instruction]>,
        /// Number of registers the task needs.
        locals_count: usize,
        /// Register indices to capture from the current task.
        captures: Arc<[usize]>,
    },
    /// Create a new list object on the heap.
    NewList { dst: usize, len: usize },
    /// Retrieve an element from a list at a specified index.
    ListGet {
        dst: usize,
        list: usize,
        index_reg: usize,
        loc: Loc,
    },
    /// Set an element in a list at a specified index.
    ListSet {
        list: usize,
        index_reg: usize,
        src: usize,
        loc: Loc,
    },
    /// Create a new object on the heap.
    NewObject { dst: usize, capacity: usize },
    /// Retrieve a property from an object by name ID.
    ObjectGet {
        dst: usize,
        obj: usize,
        name_id: u32,
        loc: Loc,
    },
    /// Set a property in an object by name ID.
    ObjectSet {
        obj: usize,
        name_id: u32,
        src: usize,
        loc: Loc,
    },
    /// Call a function (user or native) by its name ID in the string pool.
    Call {
        name_id: u32,
        args_regs: Arc<[usize]>,
        dst: Option<usize>,
        loc: Loc,
    },
    /// Call a function dynamically — the callee_reg holds a string (SSO or heap)
    /// naming either a user function or a native function, resolved at runtime.
    CallDynamic {
        callee_reg: usize,
        args_regs: Arc<[usize]>,
        dst: Option<usize>,
        loc: Loc,
    },
    /// Return from the current function with an optional value.
    Return(Option<usize>),
}

/// A compiled user-defined function.
#[derive(Debug, Clone, PartialEq)]
pub struct UserFunction {
    /// ID of the function name in the string pool.
    #[allow(dead_code)]
    pub name_id: u32,
    /// The bytecode instructions of the function.
    pub instructions: Arc<[Instruction]>,
    /// The total number of registers required by this function's stack frame.
    pub locals_count: usize,
    /// Number of parameters the function accepts.
    #[allow(dead_code)]
    pub params_count: usize,
}

/// The complete compiled program ready for execution.
#[derive(Debug, Clone, PartialEq)]
pub struct Program {
    /// Entry point bytecode.
    pub instructions: Arc<[Instruction]>,
    /// All compiled functions in the program.
    pub functions: Arc<[UserFunction]>,
    /// Global string interning pool.
    pub string_pool: Arc<[Arc<str>]>,
    /// Registers required for the main module.
    pub locals_count: usize,
    /// Number of global variables used by the program.
    pub globals_count: usize,
}
