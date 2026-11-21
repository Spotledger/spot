export function on_call() {
  const f = (n) => f(n + 1) + 1;
  return f(0);
}
