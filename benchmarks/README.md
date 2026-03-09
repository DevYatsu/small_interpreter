# Benchmarks

This folder compares **YatsuScript** against:

- Python (`python3`)
- V8 via Node.js (`node`)

The benchmark set currently covers the existing YatsuScript programs in:

- [`examples/fib.ys`](/Users/yanis/Programming/small_jit/examples/fib.ys)
- [`examples/prime.ys`](/Users/yanis/Programming/small_jit/examples/prime.ys)
- [`examples/1million_loop.ys`](/Users/yanis/Programming/small_jit/examples/1million_loop.ys)

Equivalent implementations live here:

- [`benchmarks/python/fib.py`](/Users/yanis/Programming/small_jit/benchmarks/python/fib.py)
- [`benchmarks/python/prime.py`](/Users/yanis/Programming/small_jit/benchmarks/python/prime.py)
- [`benchmarks/python/1million_loop.py`](/Users/yanis/Programming/small_jit/benchmarks/python/1million_loop.py)
- [`benchmarks/node/fib.js`](/Users/yanis/Programming/small_jit/benchmarks/node/fib.js)
- [`benchmarks/node/prime.js`](/Users/yanis/Programming/small_jit/benchmarks/node/prime.js)
- [`benchmarks/node/1million_loop.js`](/Users/yanis/Programming/small_jit/benchmarks/node/1million_loop.js)

## Prerequisites

- Rust toolchain
- `python3`
- `node`

Build the YatsuScript binary in release mode before benchmarking:

```bash
cargo build --release
```

## Run all benchmarks

```bash
python3 benchmarks/run.py
```

This uses:

- `target/release/yatsuscript`
- `python3`
- `node`

## Run a subset

Single benchmark:

```bash
python3 benchmarks/run.py fib
python3 benchmarks/run.py prime
python3 benchmarks/run.py 1million_loop
```

Single runtime:

```bash
python3 benchmarks/run.py --runtime yatsuscript
python3 benchmarks/run.py --runtime python
python3 benchmarks/run.py --runtime node
```

Custom iteration count:

```bash
python3 benchmarks/run.py --runs 10
```

## Notes

- The YatsuScript programs are executed directly from the `examples/` folder so the benchmark always measures the current canonical YatsuScript versions.
- The runner reports wall-clock timing gathered outside each process, which makes the comparison consistent across runtimes.
- For serious benchmarking, run on an otherwise idle machine and repeat enough times to smooth out noise.
