export const flatten = <T>(groups: Map<string, T[]>): T[] => {
  const flat: T[] = [];
  groups.forEach((rows) => flat.push(...rows));
  return flat;
};
