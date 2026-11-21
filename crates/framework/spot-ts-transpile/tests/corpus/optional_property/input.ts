export const total = (doc: { total?: number }): number => doc.total ?? 0;
