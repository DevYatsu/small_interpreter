# YatsuScript Language Guide

Welcome to YatsuScript! This guide will help you understand the core concepts and unique features of the language.

## 1. Syntax Overview

YatsuScript follows a minimalist, newline-based syntax. Statements are delimited by newlines, and braces `{}` are used for code blocks.

```yatsuscript
// Variable declaration and assignment
let message: "Hello, world!"
print(message)
```

## 2. Variables & Assignments

One of the most characteristic features of YatsuScript is the use of `:` for both declaration and assignment.

| Keyword | Use Case | Mutability |
|---------|----------|------------|
| `let`   | First declaration | Immutable |
| `mut`   | First declaration | Mutable |
| (none)  | Re-assigning existing | - |

```yatsuscript
let pi: 3.14              // Error if you try to re-assign pi
mut counter: 0
counter: counter + 1      // Assignment uses ':'
```

## 3. Functions

Functions are first-class citizens and support direct return values.

```yatsuscript
fn greet(name) {
    let msg: "Hello, " + name
    return msg
}

print(greet("developer"))
```

## 4. Control Flow

### If / Else
```yatsuscript
if temperature > 30 {
    print("It's hot outside!")
} else {
    print("Nice weather.")
}
```

### Loops
```yatsuscript
// While loops
mut count: 5
while count > 0 {
    print(count)
    count: count - 1
}

// For loops (with ranges)
for i in 0..10 {
    print(i)
}
```

## 5. Concurrency: `spawn`

YatsuScript makes asynchronous programming simple.

```yatsuscript
spawn {
    sleep(1000)
    print("Background task finished!")
}

print("Main script continues...")
```

## 6. Collections

### Lists
```yatsuscript
let fruits: ["apple", "banana"]
print(fruits[0])
fruits[0]: "pear"
```

### Objects
```yatsuscript
let user: {
    id: 1,
    name: "Yanis"
}
print(user.name)
```

For more details on built-in functions, see [Standard Library Reference](stdlib.md).
