export type Ticket = {
  code: string;
  openedAt: number;
  closedAt?: number;
};

export function duration(ticket: Ticket, now: number): number {
  const finish = ticket.closedAt ?? now;
  const elapsed = finish - ticket.openedAt;
  if (elapsed < 0) {
    throw new Error("finish precedes opening");
  }
  return elapsed;
}

export function olderFirst(left: Ticket, right: Ticket): number {
  return left.openedAt - right.openedAt;
}
