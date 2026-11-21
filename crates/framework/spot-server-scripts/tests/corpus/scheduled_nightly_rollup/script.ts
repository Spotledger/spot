export function on_run({ ctx }) {
  host.insert({ doctype: "log_entry", values: { message: "rollup", reference: ctx.deduplication_key } });
  host.log({ level: "info", message: ctx.scheduled_for });
}
