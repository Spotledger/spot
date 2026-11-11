import { audit } from "spot:audit";
export async function validate(doc) {
	const problems = [];
	const lines = doc.get("lines") ?? [];
	let running = 0;
	for (const line of lines) {
		if (!line.account) {
			problems.push("line without account");
		}
		running += line.amount.value;
	}
	if (running > 5e5) {
		problems.push(`total ${running} over ${5e5}`);
	}
	if (problems.length > 0) {
		audit("document.rejected", {
			id: doc.id,
			problems
		});
	}
	return problems;
}
export const enabled = true;
