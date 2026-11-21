export function* countTo(limit: number): Generator<number> {
  for (let step = 1; step <= limit; step += 1) {
    yield step;
  }
}
