const alsoEval = eval;

export const boom = (code: string): unknown => alsoEval(code);
