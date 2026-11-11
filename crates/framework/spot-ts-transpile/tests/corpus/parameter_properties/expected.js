export class Counter {
	name;
	start;
	step;
	constructor(name, start = 0, step = 1) {
		this.name = name;
		this.start = start;
		this.step = step;
	}
	next() {
		this.start += this.step;
		return this.start;
	}
	label() {
		return this.name;
	}
}
