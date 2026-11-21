import { request, respond } from "spot:http";
import { users, type User } from "spot:users";

type SearchBody = { term: string; limit?: number };

export default async function handler(): Promise<void> {
  const body = (await request.json()) as SearchBody;
  const limit = body.limit ?? 20;
  const rows = await users.search<User>(body.term, limit);
  respond.json({
    term: body.term,
    total: rows.length,
    names: rows.map((row) => row.displayName ?? row.name),
  });
}