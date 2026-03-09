#!/usr/bin/env python3

ANSWER = 78_498
MAX_NUMBER_TO_CHECK = 1_000_000

prime_mask = [True] * (MAX_NUMBER_TO_CHECK + 1)
prime_mask[0] = False
prime_mask[1] = False

total_primes_found = 0

for p in range(2, MAX_NUMBER_TO_CHECK + 1):
    if not prime_mask[p]:
        continue

    total_primes_found += 1

    for i in range(2 * p, MAX_NUMBER_TO_CHECK + 1, p):
        prime_mask[i] = False

if total_primes_found != ANSWER:
    raise SystemExit(f"wrong answer: expected {ANSWER}, got {total_primes_found}")
