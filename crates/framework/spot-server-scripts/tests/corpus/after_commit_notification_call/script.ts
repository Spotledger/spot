export function on_after_commit({ ctx }) {
  host.call({ doctype: "sales_order", id: ctx.id, method: "notify", args: { kind: "saved" } });
}
