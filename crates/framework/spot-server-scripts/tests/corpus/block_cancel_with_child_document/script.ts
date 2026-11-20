export function on_before_cancel({ doc }) {
  const shipments = host.query({ doctype: "shipment", fields: ["id"], filter: { sales_order: doc.id }, limit: 1 });
  if (shipments.length > 0) {
    host.throw({ code: "corpus.has_shipment", message: "Cancel the shipment first." });
  }
}
