interface Rows<T> { rows: T[]; total: number }

export const head = <T>(rows: Rows<T>): T | undefined => rows.rows[0];
