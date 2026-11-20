export function on_call({ args }) {
  const rows = host.query({ doctype: "sales_order", fields: ["id", "title"], filter: { customer: args.customer } });
  return { orders: rows };
}
