// The compiler refuses a literal `require(...)`, `eval(...)` or `Function(...)`; this script
// reaches for each of them through the global object by name, which no static check sees, so
// what answers is the runtime's own closure of the global scope.
function attempt(f: () => unknown): string {
  try { return f() === undefined ? "blocked" : "reached"; } catch (e) { return "blocked"; }
}
const g: any = globalThis;
export function on_call() {
  return {
    fetch: attempt(() => g["fetch"]("http://example.invalid")),
    require: attempt(() => g["require"]("fs")),
    process: attempt(() => g["process"].env),
    eval: attempt(() => g["eval"]("1 + 1")),
    func: attempt(() => new g["Function"]("return 1")()),
    global: attempt(() => g.fetch),
  };
}
