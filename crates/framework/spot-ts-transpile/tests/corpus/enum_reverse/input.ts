export enum Direction { North = 1, South = 2 }

export const name = (code: number): string => Direction[code] ?? "unknown";
