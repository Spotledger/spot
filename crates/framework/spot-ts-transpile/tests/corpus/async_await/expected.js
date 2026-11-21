export async function retry(task, times) {
	for (let attempt = 1; attempt <= times; attempt += 1) {
		try {
			return await task();
		} catch (error) {
			if (attempt === times) {
				throw error;
			}
		}
	}
	throw new Error("unreachable");
}
