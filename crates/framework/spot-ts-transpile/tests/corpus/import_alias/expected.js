import { findMany as list } from "spot:documents";
export const ids = async (tenant) => (await list(tenant)).map((d) => d.id);
