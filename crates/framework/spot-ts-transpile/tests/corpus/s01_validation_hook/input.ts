import type { Document } from "spot:documents";
import { audit } from "spot:audit";

type Line = { account: string; amount: { value: number; currency: string } };

const enum Limits { SingleLine = 100_000, Whole = 500_000 }

export async function validate(doc: Document): Promise<string[]> {
  const problems: string[] = [];
  const lines = (doc.get("lines") ?? []) as Line[];
  let running = 0;
  for (const line of lines) {
    if (!line.account) {
      problems.push("line without account");
    }
    running += line.amount.value;
  }
  if (running > Limits.Whole) {
    problems.push(`total ${running} over ${Limits.Whole}`);
  }
  if (problems.length > 0) {
    audit("document.rejected", { id: doc.id, problems });
  }
  return problems;
}

export const enabled = true;