# YatsuScript

> A register-based bytecode interpreter with a generational GC, async task execution, and a custom LSP.

YatsuScript is a lightweight scripting language designed for simplicity, performance, and modern developer experience. It features a register-based VM, NaN-boxed values, and true parallel execution via Tokio.

## Workspace Structure

The project is organized into modular crates:

- **[`ys-core`](ys-core/README.md)**: The linguistic frontend (Lexer, Parser, Compiler).
- **[`ys-runtime`](ys-runtime/README.md)**: The execution engine (VM, Heap, GC, Natives).
- **[`ys-cli`](ys-cli/README.md)**: The user-facing tool (`yatsuscript`) with REPL and Formatter.
- **[`yatsuscript-lsp`](yatsuscript-lsp/README.md)**: Language Server Protocol for IDE features.

## Why YatsuScript?

- **Simple Syntax**: Clean, newline-terminated statements with `:` for assignment.
- **Async First**: First-class `spawn { ... }` blocks for effortless concurrency.
- **Modern Tooling**: Built-in code formatter and a full LSP for highlighting/diagnostics.
- **Efficient Memory**: NaN-boxed 64-bit values and a generational, concurrent GC.

## Quick Start

### Installation

```bash
cargo install --path ys-cli
```

### Running a script

```bash
yatsuscript examples/fib.ys
```

### Starting the REPL

```bash
yatsuscript
```

## Language Features

### Variable Declarations
```yatsuscript
let x: 10      // Immutable
mut y: 20      // Mutable
y: x + y       // Assignment
```

### Concurrency
```yatsuscript
spawn {
    print("Running in parallel!")
}
```

### Collections
```yatsuscript
let list: [1, 2, 3]
let user: { name: "Yanis", age: 30 }
```

### Functional Style
```yatsuscript
let r: (0..10).step(2)
for i in r {
    print(i)
}
```

## Documentation

- **[Language Guide](docs/language_guide.md)**
- **[Standard Library Reference](docs/stdlib.md)**
- **[Internal Architecture](docs/architecture.md)**

## License

MIT © Yanis
