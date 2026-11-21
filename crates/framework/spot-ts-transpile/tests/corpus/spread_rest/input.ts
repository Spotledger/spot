export const combine = (base: { id: string }, ...rest: Record<string, unknown>[]): { id: string } => ({
  ...base,
  id: base.id,
  ...Object.assign({}, ...rest),
});
