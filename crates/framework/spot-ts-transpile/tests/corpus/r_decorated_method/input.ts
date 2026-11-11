function logged(target: unknown, key: string, desc: unknown): void {}

export class Runner {
  @logged
  start(): void {}
}
