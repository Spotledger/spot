import { request, respond } from "spot:http";
import { users } from "spot:users";
export default async function handler() {
	const body = await request.json();
	const limit = body.limit ?? 20;
	const rows = await users.search(body.term, limit);
	respond.json({
		term: body.term,
		total: rows.length,
		names: rows.map((row) => row.displayName ?? row.name)
	});
}
