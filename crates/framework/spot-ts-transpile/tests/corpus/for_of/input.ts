export const sum = (rows: number[]): number => {
  let total = 0;
  for (const row of rows) {
    total += row;
  }
  return total;
};
