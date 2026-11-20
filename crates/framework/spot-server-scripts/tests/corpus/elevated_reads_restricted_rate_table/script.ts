export function on_call({ args }) {
  const table = host.get({ doctype: "rate_table", id: args.table });
  return { rate: table.rate };
}
