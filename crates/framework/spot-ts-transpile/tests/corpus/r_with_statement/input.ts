const scope = { name: "doc" };

export const read = (): string => {
  let label: string;
  with (scope) {
    label = name;
  }
  return label;
};
