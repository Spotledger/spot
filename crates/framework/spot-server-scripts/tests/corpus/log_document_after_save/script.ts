export function on_after_save({ doc, ctx }) {
  host.insert({ doctype: "log_entry", values: { message: "saved " + doc.title, reference: ctx.id } });
}
