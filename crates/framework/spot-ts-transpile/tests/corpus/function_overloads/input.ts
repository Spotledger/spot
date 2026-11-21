function find(key: string): string | undefined;
function find(key: number): string;
function find(key: unknown): string | undefined {
  return typeof key === "number" ? String(key) : key as string | undefined;
}

export { find };
