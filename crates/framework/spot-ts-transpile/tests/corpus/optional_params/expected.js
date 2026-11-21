export function log(message, level) {
	const prefix = level ?? "info";
	console.log(`${prefix}: ${message}`);
}
