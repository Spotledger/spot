export function on_before_save({ doc }) {
  const qty = Number(doc.qty || 0);
  const rate = Number(doc.rate);
  const amount = host.round({ value: String(qty * rate), scale: 2 });
  const total = host.round({ value: String(Number(amount) * (1 + Number(doc.tax_rate) / 100)), scale: 2 });
  let unused = 1;
  return { patch: { amount: amount, total: total } };
}
