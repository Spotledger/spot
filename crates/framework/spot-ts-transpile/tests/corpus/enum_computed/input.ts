export enum Plan { Small = 1 << 0, Large = 1 << 1 }

export const chosen = (size: Plan): number => size & Plan.Large;
