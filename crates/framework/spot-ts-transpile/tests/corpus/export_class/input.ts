export class Cache<T> {
  private entries = new Map<string, T>();

  put(key: string, value: T): void {
    this.entries.set(key, value);
  }
}
