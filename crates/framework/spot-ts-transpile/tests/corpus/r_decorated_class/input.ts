function frozen(target: unknown): unknown {
  return target;
}

@frozen
export class Report {
  readonly id = "r1";
}
