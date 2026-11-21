export class Thermometer {
  private celsius = 0;

  get temperature(): number {
    return this.celsius;
  }

  set temperature(value: number) {
    this.celsius = value;
  }
}
