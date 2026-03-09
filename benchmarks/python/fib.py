#!/usr/bin/env python3

TARGET = 28
REPEAT = 5
ANSWER = 317_811


def fib(n: int) -> int:
    if n < 2:
        return n
    return fib(n - 1) + fib(n - 2)


result = 0
for _ in range(REPEAT):
    result = fib(TARGET)

if result != ANSWER:
    raise SystemExit(f"wrong answer: expected {ANSWER}, got {result}")
