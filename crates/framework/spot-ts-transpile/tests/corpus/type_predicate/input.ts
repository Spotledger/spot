export const isFilled = (value: string | undefined): value is string => typeof value === "string" && value.length > 0;
