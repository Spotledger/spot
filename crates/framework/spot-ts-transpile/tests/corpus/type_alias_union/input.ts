export type Kind = "invoice" | "quote" | "order";

export const label = (kind: Kind): string => kind.toUpperCase();
