export const safe = (task) => {
	try {
		task();
		return "done";
	} catch (error) {
		return error instanceof Error ? error.message : "failed";
	}
};
