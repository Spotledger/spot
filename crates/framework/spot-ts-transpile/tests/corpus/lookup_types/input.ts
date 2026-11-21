export type Access = { read: 1; write: 2; admin: 3 };
export type Flag = Access[keyof Access];

export const level = (word: "write"): number => { const flags: Access = { read: 1, write: 2, admin: 3 }; return flags[word]; };
