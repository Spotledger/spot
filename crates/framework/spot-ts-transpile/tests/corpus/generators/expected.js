export function* countTo(limit) {
	for (let step = 1; step <= limit; step += 1) {
		yield step;
	}
}
