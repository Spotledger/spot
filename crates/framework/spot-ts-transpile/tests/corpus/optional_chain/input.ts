export const name = (doc?: { owner?: { name?: string } }): string => doc?.owner?.name ?? "";
