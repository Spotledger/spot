export const factory = (code: string): unknown => new Function("x", code);
