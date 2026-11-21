enum Bad {
  Fine = 1,
  Broken = (() => {
    throw new Error("enum member");
  })(),
}

export function in_enum(_: unknown): number {
  return Bad.Fine;
}
