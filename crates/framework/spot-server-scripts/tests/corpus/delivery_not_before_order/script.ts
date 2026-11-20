export function on_validate({ doc }) {
  if (doc.order_date && doc.delivery_date && doc.delivery_date < doc.order_date) {
    host.throw({ code: "corpus.dates", message: "Delivery before order.",
                 fields: [{ field: "delivery_date", code: "corpus.dates", message: "Must not be before the order date." }] });
  }
}
