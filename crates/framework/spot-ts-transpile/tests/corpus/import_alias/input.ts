import { findMany as list } from "spot:documents";

export const ids = async (tenant: string): Promise<string[]> => (await list(tenant)).map((d) => d.id);
