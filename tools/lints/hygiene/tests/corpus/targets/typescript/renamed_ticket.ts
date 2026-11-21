export type Visit = {
  code: string;
  openedAt: number;
  closedAt?: number;
};
export function span(visit: Visit, current: number): number {
  const finish = visit.closedAt ?? current;
  const elapsed = finish - visit.openedAt;
  if (elapsed < 0) { throw new Error("finish precedes opening"); }
  return elapsed;
}
export function earlier(left: Visit, right: Visit): number {
  return left.openedAt - right.openedAt;
}
