interface DocOptions {
  readonly id: string;
  total?: number;
  tags?: string[];
}

export const describe = (options: DocOptions): string => `${options.id}:${options.total ?? 0}`;
