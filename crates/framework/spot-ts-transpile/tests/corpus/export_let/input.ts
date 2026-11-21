export let attempts = 0;

export const bump = (): number => ++attempts;
