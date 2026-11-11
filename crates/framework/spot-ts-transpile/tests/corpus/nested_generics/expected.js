export const flatten = (groups) => {
	const flat = [];
	groups.forEach((rows) => flat.push(...rows));
	return flat;
};
