const ANSWER = 78498;
const MAX_NUMBER_TO_CHECK = 1_000_000;

const primeMask = new Array(MAX_NUMBER_TO_CHECK + 1).fill(true);
primeMask[0] = false;
primeMask[1] = false;

let totalPrimesFound = 0;

for (let p = 2; p <= MAX_NUMBER_TO_CHECK; p += 1) {
  if (!primeMask[p]) {
    continue;
  }

  totalPrimesFound += 1;

  for (let i = 2 * p; i <= MAX_NUMBER_TO_CHECK; i += p) {
    primeMask[i] = false;
  }
}

if (totalPrimesFound !== ANSWER) {
  throw new Error(`wrong answer: expected ${ANSWER}, got ${totalPrimesFound}`);
}
