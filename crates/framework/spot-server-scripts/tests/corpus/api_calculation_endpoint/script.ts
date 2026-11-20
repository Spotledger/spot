export function on_call({ args }) {
  const gross = args.qty * args.rate;
  const discount = args.qty >= 100 ? 0.05 : 0;
  return { total: host.round({ value: String(gross * (1 - discount)), scale: 2 }) };
}
