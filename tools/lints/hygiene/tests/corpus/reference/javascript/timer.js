export function elapsed(start, finish, budget) {
  const delta = finish - start;
  if (delta < 0) {
    return 0;
  }
  if (delta > budget) {
    return budget;
  }
  return Math.round(delta);
}

export function labelElapsed(value) {
  if (!Number.isFinite(value)) {
    return "unknown";
  }
  return `${Math.floor(value)} ms`;
}
