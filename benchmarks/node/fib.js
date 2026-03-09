const TARGET = 28;
const REPEAT = 5;
const ANSWER = 317811;

function fib(n) {
  if (n < 2) {
    return n;
  }
  return fib(n - 1) + fib(n - 2);
}

let result = 0;
for (let i = 0; i < REPEAT; i += 1) {
  result = fib(TARGET);
}

if (result !== ANSWER) {
  throw new Error(`wrong answer: expected ${ANSWER}, got ${result}`);
}
