export function on_before_save({ doc }) {
  const cust = host.get({ doctype: "customer", id: doc.customer });
  return { patch: { price_list: cust.price_list, address: cust.address, title: cust.name + " / " + doc.order_date } };
}
