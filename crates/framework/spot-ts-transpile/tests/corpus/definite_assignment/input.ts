export function count(ready: boolean): number {
  let total!: number;
  if (ready) {
    total = 1;
  } else {
    total = 0;
  }
  return total;
}
