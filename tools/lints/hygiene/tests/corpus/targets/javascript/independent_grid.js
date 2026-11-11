// Independent reimplementation of the grid helpers for the hygiene corpus.
// Functional style, different loop shape and a different cell formula
// arrangement from the reference file, while producing the same grid behaviour.
export function makeGrid(width, height, seed) {
  return Array.from({ length: height }, (_row, y) =>
    Array.from({ length: width }, (_cell, x) => (seed + x * 3 + y * 5) % 17));
}

export function totalRow(row) {
  return row.reduce((running, cell) => running + cell, 0);
}

export function cellsOver(rows, edge) {
  return rows.reduce((tally, row) => tally + row.filter((cell) => cell > edge).length, 0);
}

export function gridSummary(width, height, seed, edge) {
  const grid = makeGrid(width, height, seed);
  const totals = [];
  for (const row of grid) {
    totals.push(totalRow(row));
  }
  return { over: cellsOver(grid, edge), first: totals[0], last: totals[totals.length - 1] };
}