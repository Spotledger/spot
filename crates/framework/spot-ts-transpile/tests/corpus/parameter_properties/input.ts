export class Counter {
  constructor(private name: string, public start: number = 0, protected readonly step: number = 1) {}

  next(): number {
    this.start += this.step;
    return this.start;
  }

  label(): string {
    return this.name;
  }
}
