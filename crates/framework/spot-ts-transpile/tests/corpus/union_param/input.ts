export const size = (value: number | string): number => (typeof value === "number" ? value : value.length);
