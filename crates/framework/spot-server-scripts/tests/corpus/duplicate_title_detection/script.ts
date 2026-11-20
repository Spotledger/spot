export function on_validate({ doc }) {
  const same = host.query({ doctype: "sales_order", fields: ["id"], filter: { title: doc.title }, limit: 1 });
  if (same.length > 0) {
    host.throw({ code: "corpus.duplicate", message: "Already entered.",
                 fields: [{ field: "title", code: "corpus.duplicate", message: "Duplicate title." }] });
  }
}
