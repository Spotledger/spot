export const level = (word) => {
	const flags = {
		read: 1,
		write: 2,
		admin: 3
	};
	return flags[word];
};
