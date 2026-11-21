type Shape = { kind: string };

const visible = { kind: "square", area: 4 } satisfies Shape & { area: number };

export const show = (): string => visible.kind;
