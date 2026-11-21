export function on_call() {
  host.log({ level: "info", message: "x".repeat(1000000) });
  return { done: true };
}
