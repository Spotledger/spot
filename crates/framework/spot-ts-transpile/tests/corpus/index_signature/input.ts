interface Bag { [key: string]: unknown; known: number }

export const pick = (bag: Bag): unknown => bag["known"];
