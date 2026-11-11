export const combine = (base, ...rest) => ({
	...base,
	id: base.id,
	...Object.assign({}, ...rest)
});
