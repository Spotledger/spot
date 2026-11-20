// Own inputs of the corpus run (WP-EXT-08.T3): small programs with callable entry points, one per
// language feature the engine must run the way the TypeScript says. Read by the test, never edited
// by it. Every entry takes one JSON argument and returns JSON.

class Acc {
  static readonly BASE = 5;
  total = 10;
  #hidden = 2;
  add(x: number): number {
    this.total += x * this.#hidden;
    return this.total + Acc.BASE;
  }
}

export function class_fields_run(n: number): number {
  return new Acc().add(n);
}

class Point {
  constructor(public x: number, private y: number = 4, readonly tag: string = "p") {}
  sum(): number {
    return this.x + this.y;
  }
}

export function parameter_properties_run(x: number): unknown {
  const p = new Point(x);
  return { sum: p.sum(), x: p.x, tag: p.tag, ownKeys: Object.keys(p) };
}

abstract class Shape {
  abstract kind(): string;
  describe(): string {
    return `shape:${this.kind()}`;
  }
}

class Square extends Shape {
  kind(): string {
    return "square";
  }
}

export function abstract_run(_: unknown): string {
  return new Square().describe();
}

export function abstract_base_call(_: unknown): string {
  const Base = Shape as unknown as new () => Shape;
  try {
    return new Base().kind();
  } catch (error) {
    return (error as Error).name;
  }
}

enum Dir {
  Up = "up",
  Down = "down",
}
enum Num {
  A,
  B,
  C = 10,
}
const enum Inline {
  X = 7,
  Y = X + 1,
}

export function enums_run(_: unknown): unknown {
  return { s: Dir.Down, n: Num.B, c: Num.C, reverse: Num[1], inline: Inline.Y, names: Object.keys(Dir) };
}

function id<T>(value: T): T {
  return value;
}

export function generics_and_assertions_run(x: unknown): number {
  const n = id<number>(x as number);
  const maybe: number | undefined = n;
  return maybe! + 1;
}

export function map_for_of_run(_: unknown): number {
  const m = new Map<string, number>([["a", 1], ["bb", 2]]);
  let total = 0;
  for (const [key, value] of m) {
    total += value + key.length;
  }
  return total;
}

function* upto(limit: number): Generator<number> {
  for (let i = 1; i <= limit; i += 1) {
    yield i;
  }
}

export function generator_run(n: number): number[] {
  return [...upto(n)];
}

async function double(x: number): Promise<number> {
  await Promise.resolve();
  return x * 2;
}

export async function async_run(n: number): Promise<number> {
  const first = await double(n);
  return await double(first) + 1;
}

export function destructuring_run(input: { a?: number; b?: number; c?: { d?: number } }): number[] {
  const { a = 1, b = 2, c: { d = 3 } = {} } = input;
  return [a, b, d];
}

class Thermo {
  private celsius = 1;
  get temperature(): number {
    return this.celsius;
  }
  set temperature(value: number) {
    this.celsius = value * 2;
  }
}

export function accessors_run(v: number): number {
  const t = new Thermo();
  t.temperature = v;
  return t.temperature;
}

class Counter {
  static count = 0;
  static {
    Counter.count = 40;
  }
  static inc(): number {
    Counter.count += 2;
    return Counter.count;
  }
}

export function static_members_run(_: unknown): number {
  return Counter.inc();
}

export const arrow_run = (x: number): number => x * x;

export async function retry_run(times: number): Promise<number> {
  let attempts = 0;
  const task = async (): Promise<number> => {
    attempts += 1;
    if (attempts < times) {
      throw new Error("again");
    }
    return attempts;
  };
  for (;;) {
    try {
      return await task();
    } catch (error) {
      if (attempts >= 10) {
        throw error;
      }
    }
  }
}

export function tagged_run(_: unknown): string {
  const tag = (parts: TemplateStringsArray, ...values: unknown[]): string =>
    String.raw({ raw: parts.raw }, ...values).toUpperCase();
  return tag`a${1}b${2}`;
}

export function catch_unknown_run(_: unknown): string {
  const safe = (task: () => void): string => {
    try {
      task();
      return "done";
    } catch (error: unknown) {
      return error instanceof Error ? error.message : "failed";
    }
  };
  return safe(() => {
    throw new Error("caught");
  });
}
