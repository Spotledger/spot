export function on_validate({ doc }) {
  const cust = host.get({ doctype: "customer", id: doc.customer });
  if (Number(doc.total) + Number(cust.outstanding) > Number(cust.credit_limit)) {
    host.throw({ code: "corpus.credit_limit", message: "Over the credit limit.",
                 fields: [{ field: "total", code: "corpus.credit_limit", message: "Exceeds the customer's limit." }] });
  }
}
