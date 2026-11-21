import { findMany } from "spot:documents";
export const first = async (id) => (await findMany(id, 1)).rows[0];
