import { Document, findMany } from "spot:documents";

export const first = async (id: string): Promise<Document | undefined> => (await findMany(id, 1)).rows[0];
