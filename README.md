# YatsuScript

`yatsuscript` is a Rust implementation of **YatsuScript**: a small, register-based bytecode interpreter with a custom runtime, NaN-boxed values, a managed heap, and async task execution on Tokio.

The current implementation is a fast interpreter rather than a machine-code JIT.

## What this project does

YatsuScript is a lightweight scripting language designed around:

- simple syntax
- newline-terminated statements
- mutable and immutable variables
- functions and recursion
- lists and objects
- ranges and `for` loops
- async task spawning
- a small set of built-in native functions

The pipeline is:

1. Source text is tokenized by [`logos`](https://crates.io/crates/logos).
2. The parser compiles tokens directly into bytecode.
3. The interpreter executes that bytecode on a register VM.
4. Heap-allocated values live in a managed heap with a generational GC.

## Running it

Build and run a file:

```bash
cargo run -- examples/fib.ys
```

Start the REPL:

```bash
cargo run
```

Format `.ys` files:

```bash
cargo run -- fmt
cargo run -- fmt examples/fib.ys
```

## High-level architecture

### 1. Lexer

The lexer lives in [`src/lexer.rs`](/src/lexer.rs). It recognizes:

- keywords like `let`, `mut`, `fn`, `if`, `while`, `for`, `spawn`
- literals: numbers, booleans, strings, template strings
- punctuation and operators
- comments

Important lexer rules:

- Source is expected to be ASCII.
- Newlines are significant tokens.
- Spaces and tabs are ignored.
- `// ...` line comments are supported.
- `/* ... */` block comments are supported and are not nested.

### 2. Parser and compiler

The parser in [`src/parser.rs`](/src/parser.rs) performs parsing and bytecode generation in one pass.

It compiles source into a [`Program`](/src/compiler.rs) containing:

- a top-level instruction stream
- compiled user functions
- an interned string pool
- counts for local and global registers

This is a register VM, not a stack VM. Expressions compile into numbered registers, and bytecode instructions read and write those registers directly.

### 3. Runtime / interpreter

The interpreter is in [`src/backends/interpreter.rs`](/src/backends/interpreter.rs).

Runtime characteristics:

- values are stored in a 64-bit `Value` using NaN-boxing
- numbers are plain `f64`
- booleans and object references are tagged inside the same 64-bit word
- short strings up to 6 bytes may be stored inline using SSO
- longer strings, lists, objects, ranges, timestamps, and bound methods live on the heap
- user code executes on bytecode instructions like `Add`, `ListGet`, `ObjectSet`, `Call`, `Spawn`, and `JumpIfFalse`

### 4. Concurrency

`spawn { ... }` compiles to a dedicated bytecode instruction and runs the block as an async Tokio task.

Key detail: spawned blocks can capture already-allocated registers from the surrounding scope. Globals are also shared. The runtime uses atomics extensively, so shared globals and many heap slots are stored in atomic cells.

## CLI behavior

The CLI entry point is [`src/main.rs`](/src/main.rs), and user-facing command handling is in [`src/cli.rs`](/src/cli.rs).

Behavior:

- `yatsuscript [FILE]` runs a `.ys` file
- `yatsuscript` starts the REPL
- `yatsuscript fmt [FILES...]` formats `.ys` files

## YatsuScript syntax

This section reflects what the current parser accepts.

### Statements are newline-based

YatsuScript does not use semicolons. Statements are separated by newlines.

```pi
let x: 1
mut y: 2
print(x + y)
```

Blocks use braces:

```pi
if x < y {
    print("x is smaller")
}
```

### Comments

```pi
// line comment

/* block comment */
```

### Variables

Immutable and mutable declarations:

```pi
let answer: 42
mut counter: 0
```

Assignment also uses `:`:

```pi
counter: counter + 1
```

This is one of the unusual parts of YatsuScript:

- `let name: expr` declares an immutable variable
- `mut name: expr` declares a mutable variable
- `name: expr` assigns to an existing variable, list element, or object field

Examples:

```pi
let a: 1
mut b: 2
b: a + b
```

Indexed and field assignment:

```pi
list[0]: 10
user.name: "yanis"
user.profile.age: 30
```

### Literals

#### Numbers

Numbers are parsed as `f64`.

```pi
let a: 1
let b: 3.14
let c: 1_000_000
let d: -42
```

Notes:

- numeric separators with `_` are allowed
- negative numeric literals are accepted
- there is no separate unary minus operator implementation; negative values are primarily handled as literals

#### Booleans

```pi
let a: true
let b: false
```

#### Strings

Double-quoted strings support escapes such as `\n`, `\r`, `\t`, `\\`, `\"`, and `\uXXXX`.

```pi
let s: "hello"
let path: "line 1\nline 2"
```

#### Template strings

Backtick strings support `${...}` interpolation. Each embedded expression is evaluated and stringified with `str(...)`.

```pi
let name: "YatsuScript"
print(`hello ${name}`)
print(`2 + 2 = ${2 + 2}`)
```

### Collections

#### Lists

```pi
let xs: [1, 2, 3]
print(xs[1])
xs[1]: 99
```

Behavior detail: list assignment can grow a list if the target index is beyond the current length.

There is also a built-in list method:

```pi
let xs: []
xs.pad(5, 0)
print(xs) // [0, 0, 0, 0, 0]
```

#### Objects

Object literals use identifier keys:

```pi
let user: {
    name: "yanis",
    age: 30
}

print(user.name)
user.age: 31
```

Current parser limitation: object literal keys must be identifiers, not arbitrary string expressions.

### Expressions and operators

Supported binary operators, from lowest to highest precedence:

1. `..`
2. `==`, `!=`
3. `<`, `<=`, `>`, `>=`
4. `+`, `-`
5. `*`, `/`

Examples:

```pi
let a: 1 + 2 * 3
let b: (1 + 2) * 3
let c: 0..10
```

Unary operator:

```pi
!expr
```

Examples:

```pi
let ok: !false
let still_ok: !!true
```

Truthiness in the current runtime:

- `false` is falsy
- numeric `0` is falsy
- `NaN` is falsy
- nonzero numbers are truthy
- strings, lists, objects, and ranges are truthy, including empty ones

There is currently no `&&`, `||`, or ternary operator.

### Ranges

Ranges are created with `start .. end`:

```pi
let r: 0..10
print(r.start)
print(r.end)
```

You can derive a stepped range with `.step(n)`:

```pi
let r: (0..10).step(2)
```

The runtime also exposes:

- `r.start`
- `r.end`
- `r.step(...)`

Important implementation detail: `for` loops currently assume a positive step and use `< end` as the loop condition. Descending iteration is not implemented correctly yet.

### Control flow

#### `if` / `else`

```pi
if x > 10 {
    print("big")
} else {
    print("small")
}
```

#### `while`

```pi
mut i: 0
while i < 10 {
    print(i)
    i: i + 1
}
```

#### `for`

`for` iterates over a range object:

```pi
for i in 0..5 {
    print(i)
}
```

Stepped example:

```pi
for i in (0..10).step(3) {
    print(i)
}
```

Loop semantics are end-exclusive.

#### `continue`

```pi
for i in 0..10 {
    if i == 5 {
        continue
    }
    print(i)
}
```

There is currently no `break`.

### Functions

Function declaration:

```pi
fn add(a, b) {
    return a + b
}
```

Function call:

```pi
print(add(1, 2))
```

Functions can return values:

```pi
fn abs_like(x) {
    if x < 0 {
        return 0 - x
    }
    return x
}
```

If a function body does not end with `return`, the compiler inserts an implicit empty return.

Arity is checked for user-defined functions at runtime.

### Function references and dynamic calls

YatsuScript has a convenient but unusual function-reference model.

If an identifier is used as an expression and is not a variable, the parser treats it like a string-valued function name. That makes this work:

```pi
fn handle(req) {
    return "handled " + req
}

fn run(cb) {
    return cb("data")
}

print(run(handle))
```

This also works with native functions:

```pi
print(str(42))
print((str)("42"))
```

Practical interpretation: function references are name-based, not closure objects.

### Calls with and without parentheses

The language accepts both:

```pi
print("hello")
print "hello"
```

Parenthesized calls are the safer general form. Bare calls are mainly useful for simple statement-style native calls like `print`.

### `spawn`

```pi
spawn {
    print("running concurrently")
}
```

Spawned blocks run asynchronously. They can read captured values and shared globals.

Example:

```pi
mut g: 0

fn task() {
    g: g + 1
}

spawn {
    task()
}
```

## Built-in functions

These are registered in [`src/backends/interpreter.rs`](/src/backends/interpreter.rs).

### `print(...)`

Prints all arguments separated by spaces, then a newline.

```pi
print("a", 1, true)
```

### `len(x)`

Returns the length of:

- strings
- lists
- objects
- ranges

```pi
print(len("abc"))
print(len([1, 2, 3]))
```

### `time()`

Returns the current Unix timestamp as a number in seconds.

### `timestamp()`

Returns a timestamp object representing `Instant::now()`.

Supported property:

- `.elapsed`

Example:

```pi
let t: timestamp()
sleep(100)
print(t.elapsed)
```

### `sleep(ms)`

Asynchronously sleeps for the given number of milliseconds.

### `fetch(url)`

Performs an HTTP GET request and prints the status and body. It does not currently return the response body to YatsuScript code.

### `serve(port, handler)`

Starts a simple TCP/HTTP server and dispatches each request to a YatsuScript function.

The handler receives the raw request text as its first argument and should return a response body string, or a full HTTP response string if it starts with `HTTP/`.

Example:

```pi
fn handle(req) {
    return "Hello"
}

serve(9000, handle)
```

### `str(x)`

Converts a value to its string representation.

Useful with numbers, booleans, lists, objects, and ranges.

## Semantics and implementation details

### Equality

`==` and `!=` are implemented in the runtime. Numbers and booleans compare by value. Heap objects are compared by runtime equality rules inside the context implementation.

### String concatenation

`+` works for:

- number + number
- string + string

Mixed string/number concatenation is not implicit. Use `str(...)`:

```pi
print("value = " + str(42))
```

### Globals vs locals

- top-level `let`/`mut` declarations become globals
- function parameters and variables declared inside functions are locals
- spawned tasks can capture existing registers

### Error reporting

Parsing and runtime errors include line and column data. The CLI prints a source snippet with a caret pointing at the error location.

### Formatter

The formatter in [`src/formatter.rs`](/src/formatter.rs) rewrites `.ys` files based on the lexer token stream. It understands the current syntax, indentation, braces, commas, and operators.

## Current language limitations

Based on the current implementation:

- no semicolons
- no `break`
- no `&&` / `||`
- no unary `-` operator distinct from negative literals
- no closures or first-class function objects beyond name-based references
- object literal keys must be identifiers
- `for` loops assume positive range steps
- `fetch` prints responses instead of returning them
- source is ASCII-oriented

## Examples

Useful sample programs:

- [`examples/fib.ys`](/examples/fib.ys)
- [`examples/prime.ys`](/examples/prime.ys)
- [`examples/test_concurrency.ys`](/examples/test_concurrency.ys)
- [`examples/test_server.ys`](/examples/test_server.ys)
- [`examples/objects.ys`](/examples/objects.ys)

## Source map

- [`src/lexer.rs`](/src/lexer.rs): tokenization
- [`src/parser.rs`](/src/parser.rs): parsing and bytecode generation
- [`src/compiler.rs`](/src/compiler.rs): bytecode and value definitions
- [`src/backends/interpreter.rs`](/src/backends/interpreter.rs): runtime and native functions
- [`src/cli.rs`](/src/cli.rs): CLI and REPL
- [`src/formatter.rs`](/src/formatter.rs): source formatter
