export class Cache {
	entries = new Map();
	put(key, value) {
		this.entries.set(key, value);
	}
}
