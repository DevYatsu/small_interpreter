#!/usr/bin/env python3

from __future__ import annotations

import argparse
import shutil
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
BENCH_ROOT = ROOT / "benchmarks"
EXAMPLES = ROOT / "examples"


@dataclass(frozen=True)
class Benchmark:
    name: str
    yatsuscript: list[str]
    python: list[str]
    node: list[str]


BENCHMARKS: dict[str, Benchmark] = {
    "fib": Benchmark(
        name="fib",
        yatsuscript=["target/release/yatsuscript", str(EXAMPLES / "fib.ys")],
        python=["python3", str(BENCH_ROOT / "python" / "fib.py")],
        node=["node", str(BENCH_ROOT / "node" / "fib.js")],
    ),
    "prime": Benchmark(
        name="prime",
        yatsuscript=["target/release/yatsuscript", str(EXAMPLES / "prime.ys")],
        python=["python3", str(BENCH_ROOT / "python" / "prime.py")],
        node=["node", str(BENCH_ROOT / "node" / "prime.js")],
    ),
    "1million_loop": Benchmark(
        name="1million_loop",
        yatsuscript=["target/release/yatsuscript", str(EXAMPLES / "1million_loop.ys")],
        python=["python3", str(BENCH_ROOT / "python" / "1million_loop.py")],
        node=["node", str(BENCH_ROOT / "node" / "1million_loop.js")],
    ),
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Benchmark YatsuScript vs Python and Node.js.")
    parser.add_argument(
        "benchmarks",
        nargs="*",
        choices=sorted(BENCHMARKS.keys()),
        help="Optional benchmark names to run.",
    )
    parser.add_argument(
        "--runtime",
        choices=["yatsuscript", "python", "node"],
        action="append",
        help="Limit execution to one or more runtimes.",
    )
    parser.add_argument(
        "--runs",
        type=int,
        default=5,
        help="Number of timed runs per benchmark/runtime pair.",
    )
    return parser.parse_args()


def ensure_runtime_available(runtime: str, command: list[str]) -> None:
    executable = command[0]
    if "/" in executable:
        path = ROOT / executable
        if not path.exists():
            raise SystemExit(
                f"Missing {runtime} executable: {path}\n"
                "Build it first with: cargo build --release"
            )
        return

    if shutil.which(executable) is None:
        raise SystemExit(f"Required executable not found in PATH: {executable}")


def run_once(command: list[str]) -> float:
    start = time.perf_counter()
    subprocess.run(
        command,
        cwd=ROOT,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    return time.perf_counter() - start


def format_seconds(seconds: float) -> str:
    return f"{seconds:.6f}s"


def main() -> int:
    args = parse_args()
    selected_benchmarks = args.benchmarks or list(BENCHMARKS.keys())
    selected_runtimes = args.runtime or ["yatsuscript", "python", "node"]

    for benchmark_name in selected_benchmarks:
        benchmark = BENCHMARKS[benchmark_name]
        print(f"\n== {benchmark.name} ==")

        for runtime in selected_runtimes:
            command = getattr(benchmark, runtime)
            ensure_runtime_available(runtime, command)

            timings = [run_once(command) for _ in range(args.runs)]
            avg = statistics.mean(timings)
            best = min(timings)
            worst = max(timings)

            print(
                f"{runtime:12} avg={format_seconds(avg)} "
                f"best={format_seconds(best)} worst={format_seconds(worst)}"
            )

    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except subprocess.CalledProcessError as exc:
        print(f"Benchmark command failed: {exc.cmd}", file=sys.stderr)
        raise SystemExit(exc.returncode) from exc
