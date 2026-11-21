export function fillGrid(width, height, seed) {
  const rows = [];
  for (let y = 0; y < height; y += 1) {
    const row = [];
    for (let x = 0; x < width; x += 1) {
      row.push((x * 3 + y * 5 + seed) % 17);
    }
    rows.push(row);
  }
  return rows;
}

export function sumRow(row) {
  return row.reduce((total, cell) => total + cell, 0);
}

export function countAbove(rows, edge) {
  let count = 0;
  for (const row of rows) {
    for (const cell of row) {
      if (cell > edge) count += 1;
    }
  }
  return count;
}
