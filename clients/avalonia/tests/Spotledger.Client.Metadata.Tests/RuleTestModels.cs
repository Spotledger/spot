using Spotledger.Client.Expressions;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// The DocType models the rule tests edit, written with the runtime's public
/// constructors: an expense claim carrying every rule kind and a child table whose rules
/// read the parent, a claim whose defaults depend on each other, and a pair of fields
/// whose rules form a cycle.
/// </summary>
internal static class RuleTestModels
{
    /// <summary>The expression version these models are written for.</summary>
    public const int Version = RuleEngine.SupportedExpressionVersion;

    /// <summary>A claim with one field per rule kind plus an untouched field.</summary>
    public static DocTypeModel Claim() => new(
        "expense_claim",
        "Expense Claim",
        version: 3,
        isPosted: false,
        fields:
        [
            new FieldModel("kind", "Kind", FieldKind.Select, options: ["Travel", "Meal"]),
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("has_receipt", "Has receipt", FieldKind.Bool),
            new FieldModel("status", "Status", FieldKind.Select, options: ["Draft", "Paid"],
                readOnlyDependsOn: "doc.status == 'Paid'"),
            new FieldModel("total", "Total", FieldKind.DecimalNumber, scale: 2, precision: 18,
                readOnly: true, readOnlyDependsOn: "doc.amount > 0.00"),
            new FieldModel("receipt_note", "Receipt note", FieldKind.Text,
                dependsOn: "doc.has_receipt"),
            new FieldModel("receipt_detail", "Receipt detail", FieldKind.Text,
                dependsOn: "doc.receipt_note != null and doc.receipt_note != ''"),
            new FieldModel("justification", "Justification", FieldKind.Text,
                dependsOn: "doc.amount > 0.00", mandatoryDependsOn: "doc.amount > 100.00"),
            new FieldModel("discount_reason", "Discount reason", FieldKind.Text,
                dependsOn: "doc.amount > 10.00"),
            new FieldModel("note", "Note", FieldKind.Text),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "expense_item"),
        ],
        childTables: new Dictionary<string, DocTypeModel>(StringComparer.Ordinal)
        {
            ["items"] = Row(),
        },
        exprVersion: Version);

    /// <summary>The claim's row DocType: rules that read the row (<c>doc</c>) and the
    /// claim (<c>parent</c>), plus a default of its own.</summary>
    public static DocTypeModel Row() => new(
        "expense_item",
        "Expense Item",
        version: 1,
        isPosted: false,
        fields:
        [
            new FieldModel("qty", "Quantity", FieldKind.DecimalNumber, scale: 2, precision: 12),
            new FieldModel("price", "Price", FieldKind.DecimalNumber, scale: 2, precision: 12),
            new FieldModel("line_total", "Line total", FieldKind.DecimalNumber, scale: 2, precision: 18,
                readOnlyDependsOn: "parent.amount > 0.00 and doc.qty > 0.00"),
            new FieldModel("note", "Note", FieldKind.Text, dependsOn: "parent.has_receipt"),
            new FieldModel("unit", "Unit", FieldKind.Text, defaultExpr: "'each'"),
        ],
        exprVersion: Version);

    /// <summary>
    /// Defaults that depend on each other, declared out of order: <c>total</c> reads
    /// <c>base</c> and <c>tax</c>, <c>tax</c> reads <c>base</c>. Evaluating in field
    /// order would give the wrong <c>total</c>; the engine evaluates their dependency
    /// order once, on creation.
    /// </summary>
    public static DocTypeModel Defaults() => new(
        "priced_order",
        "Priced Order",
        version: 1,
        isPosted: false,
        fields:
        [
            new FieldModel("total", "Total", FieldKind.DecimalNumber, scale: 2, precision: 18,
                defaultExpr: "doc.base + doc.tax"),
            new FieldModel("base", "Base", FieldKind.DecimalNumber, scale: 2, precision: 18,
                defaultExpr: "100.00"),
            new FieldModel("tax", "Tax", FieldKind.DecimalNumber, scale: 2, precision: 18,
                defaultExpr: "doc.base + 2.50"),
            new FieldModel("label", "Label", FieldKind.Text, defaultExpr: "'generated'"),
            new FieldModel("stamp", "Stamp", FieldKind.Datetime, defaultExpr: "now()"),
            new FieldModel("flag", "Flag", FieldKind.Bool, defaultExpr: "doc.base > 50.00"),
        ],
        exprVersion: Version);

    /// <summary>A claim whose own fields carry no expression at all, but whose child row
    /// DocType does: the row has its own <c>expr_version</c>, so its rules must still
    /// run even though the document scope is empty.</summary>
    public static DocTypeModel RowOnlyExpressions() => new(
        "row_only",
        "Row Only",
        version: 1,
        isPosted: false,
        fields:
        [
            new FieldModel("flag", "Flag", FieldKind.Bool),
            new FieldModel("items", "Items", FieldKind.Table, childDoctype: "row_only_item"),
        ],
        childTables: new Dictionary<string, DocTypeModel>(StringComparer.Ordinal)
        {
            ["items"] = new DocTypeModel(
                "row_only_item",
                "Row Only Item",
                version: 1,
                isPosted: false,
                fields:
                [
                    new FieldModel("note", "Note", FieldKind.Text, dependsOn: "parent.flag"),
                ],
                exprVersion: Version),
        });

    /// <summary>Two fields whose rules read each other: a cycle the engine must report
    /// rather than resolve by accident.</summary>
    public static DocTypeModel Cycle() => new(
        "cyclic",
        "Cyclic",
        version: 1,
        isPosted: false,
        fields:
        [
            new FieldModel("alpha", "Alpha", FieldKind.Text, dependsOn: "doc.beta != null"),
            new FieldModel("beta", "Beta", FieldKind.Text, dependsOn: "doc.alpha != null"),
        ],
        exprVersion: Version);

    /// <summary>A clock frozen at one instant, for the <c>now()</c> tests.</summary>
    public sealed class FixedTime(DateTimeOffset now) : TimeProvider
    {
        private readonly DateTimeOffset _now = now;

        /// <inheritdoc/>
        public override DateTimeOffset GetUtcNow() => _now;
    }

    /// <summary>A shared count, so every wrapped context of one change adds to it.</summary>
    public sealed class Reads
    {
        /// <summary>How many path reads the expressions made.</summary>
        public int Count { get; set; }
    }

    /// <summary>A context that counts the field reads it serves.</summary>
    public sealed class CountingContext(IExpressionContext inner, Reads reads) : IExpressionContext
    {
        private readonly IExpressionContext _inner = inner;
        private readonly Reads _reads = reads;

        /// <summary>The instant the wrapped context reports.</summary>
        public ExpressionDateTime Now => _inner.Now;

        /// <inheritdoc/>
        public ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names)
        {
            _reads.Count++;
            return _inner.Field(root, names);
        }
    }
}
