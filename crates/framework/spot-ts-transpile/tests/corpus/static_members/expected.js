export class Registry {
	static count = 0;
	static register() {
		Registry.count += 1;
	}
}
