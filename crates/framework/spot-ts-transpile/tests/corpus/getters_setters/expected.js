export class Thermometer {
	celsius = 0;
	get temperature() {
		return this.celsius;
	}
	set temperature(value) {
		this.celsius = value;
	}
}
