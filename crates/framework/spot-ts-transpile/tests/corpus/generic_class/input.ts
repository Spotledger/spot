export class Box<T> {
  constructor(private readonly value: T) {}

  peek(): T {
    return this.value;
  }
}
