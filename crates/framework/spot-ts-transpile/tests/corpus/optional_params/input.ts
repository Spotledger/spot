export function log(message: string, level?: string): void {
  const prefix = level ?? "info";
  console.log(`${prefix}: ${message}`);
}
