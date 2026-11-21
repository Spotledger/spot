export class Rate {
	static MAX = 100;
	#used = 0;
	level = 1;
	charge(units) {
		this.#used += units;
		return Math.min(units * this.level, Rate.MAX);
	}
}
