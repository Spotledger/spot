export function on_call({ args }) {
  const result = host.call({ doctype: "sales_order", id: args.id, method: "recalculate", args: {} });
  return { result: result };
}
