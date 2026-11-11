export const sum = (rows) => {
	let total = 0;
	for (const row of rows) {
		total += row;
	}
	return total;
};
