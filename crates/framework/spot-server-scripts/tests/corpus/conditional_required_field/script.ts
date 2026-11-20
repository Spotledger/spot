export function on_validate({ doc }) {
  if (doc.status === "Closed" && !doc.notes) {
    host.throw({ code: "corpus.notes_required", message: "Say why the order was closed.",
                 fields: [{ field: "notes", code: "corpus.notes_required", message: "Required when closed." }] });
  }
}
