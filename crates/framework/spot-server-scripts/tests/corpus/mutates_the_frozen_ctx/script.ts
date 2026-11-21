export function on_call({ ctx }) {
  (ctx as any).function = "hacked";
  return { ok: true };
}
