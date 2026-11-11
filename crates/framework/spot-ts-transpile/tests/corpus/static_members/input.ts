export class Registry {
  static count = 0;

  static register(): void {
    Registry.count += 1;
  }
}
