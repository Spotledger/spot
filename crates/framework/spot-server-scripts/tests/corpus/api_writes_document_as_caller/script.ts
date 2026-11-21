export function on_call({ args }) {
  const saved = host.insert({ doctype: "log_entry", values: { message: args.message, reference: "api" } });
  return { id: saved.id };
}
