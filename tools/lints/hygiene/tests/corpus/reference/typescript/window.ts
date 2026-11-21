export function averageWindow(values: readonly number[]): number | undefined {
  if (values.length === 0) {
    return undefined;
  }
  let total = 0;
  for (const value of values) {
    total += value;
  }
  return total / values.length;
}

export function withinWindow(value: number, edge: number): boolean {
  return value >= -edge && value <= edge;
}

export function centerWindow(values: readonly number[]): number[] {
  const middle = averageWindow(values);
  if (middle === undefined) return [];
  return values.map((value) => value - middle);
}
