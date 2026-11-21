export function on_validate({ doc, ctx }) {
  // Wanted: the cap depends on ctx.user's roles. There is no ctx.user here.
  const cap = ctx.user ? 20 : 10;
  if (Number(doc.discount) > cap) {
    host.throw({ code: "corpus.discount", message: "Discount too high.",
                 fields: [{ field: "discount", code: "corpus.discount", message: "Above the cap." }] });
  }
}
