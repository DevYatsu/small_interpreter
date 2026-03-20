# YatsuScript Standard Library Reference

This reference lists all the built-in functions available in the YatsuScript environment.

## I/O Core

### `print(...)`
Prints all arguments, separated by spaces, followed by a newline.

```yatsuscript
print("Value is:", 42) // "Value is: 42\n"
```

### `str(x)`
Converts any value `x` to its string representation.

```yatsuscript
print(str(1 + 2)) // "3"
```

## System & Utilities

### `len(x)`
Returns the number of elements in a list, characters in a string, or fields in an object.

```yatsuscript
len("Yatsu") // 5
len([1, 2, 3]) // 3
```

### `sleep(ms)`
Pauses the current task for `ms` milliseconds without blocking other parallel `spawn` tasks.

```yatsuscript
sleep(1000) // 1 second
```

### `time()`
Returns the number of seconds since the Unix epoch.

```yatsuscript
print(time()) // 1711234567.89
```

### `timestamp()`
Returns an opaque `Timestamp` object.

| Property | Description |
|----------|-------------|
| `.elapsed` | Number of seconds since the timestamp was created |

```yatsuscript
let t: timestamp()
sleep(500)
print(t.elapsed) // ~0.5
```

## Networking

### `fetch(url)`
Performs an HTTP GET request and prints the status and body.

```yatsuscript
fetch("https://api.github.com/repos/DevYatsu/YatsuScript")
```

### `serve(port, handler)`
Starts a simple HTTP server on the specified port. The `handler` is a function that receives the raw request string and should return a response string.

```yatsuscript
fn handle(req) {
    return "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nHello from YatsuScript!"
}

serve(8080, handle)
```

## Collection Built-ins

### `list.pad(length, value)`
Pads a list to the specified `length` by appending the given `value`.

```yatsuscript
mut xs: [1, 2]
xs.pad(5, 0) // [1, 2, 0, 0, 0]
```

### `range.step(n)`
Derives a new range with a custom step size.

```yatsuscript
let r: (0..10).step(2)
for i in r {
    print(i) // 0, 2, 4, 6, 8
}
```
