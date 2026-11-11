export const shout = (parts: TemplateStringsArray, ...values: unknown[]): string =>
  String.raw({ raw: parts.raw }, ...values).toUpperCase();
