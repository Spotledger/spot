export function on_call() {
  for (let i = 0; i < 10000; i++) { host.round({ value: "1.005", scale: 2 }); }
}
