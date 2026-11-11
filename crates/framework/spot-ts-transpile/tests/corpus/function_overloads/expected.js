function find(key) {
	return typeof key === "number" ? String(key) : key;
}
export { find };
