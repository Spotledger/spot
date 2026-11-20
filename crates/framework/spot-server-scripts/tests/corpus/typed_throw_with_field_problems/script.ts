export function on_validate({ doc }) {
  const problems = [];
  if (!(Number(doc.qty) > 0)) problems.push({ field: "qty", code: "corpus.qty", message: "Quantity must be positive." });
  if (!(Number(doc.rate) > 0)) problems.push({ field: "rate", code: "corpus.rate", message: "Rate must be positive." });
  if (problems.length) host.throw({ code: "corpus.invalid", message: "Fix the highlighted fields.", fields: problems });
}
