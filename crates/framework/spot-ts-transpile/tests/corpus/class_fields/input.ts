export class Rate {
  static readonly MAX = 100;
  #used = 0;
  protected level: number = 1;

  charge(units: number): number {
    this.#used += units;
    return Math.min(units * this.level, Rate.MAX);
  }
}
