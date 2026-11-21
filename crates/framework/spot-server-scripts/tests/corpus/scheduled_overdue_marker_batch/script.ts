export function on_run() {
  for (let i = 0; i < 500; i++) {
    host.round({ value: String(i), scale: 2 });
  }
}
