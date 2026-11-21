const enum Level { Low = 1, High = 2 }

export const rank = (value: Level): string => (value === Level.High ? "high" : "low");
