using System.Text.Json;

namespace Spotledger.Client.Metadata;

/// <summary>
/// The <c>doc</c> and <c>parent</c> maps a lookup request carries (the contract's
/// lookup body: <c>doc</c> is the current document, <c>parent</c> the document above a
/// child row, both in the value encoding). The client sends these and nothing else: the
/// **server** applies the field's filter over exactly them (ADR-068 §3 and amendment
/// A3.2), so the client evaluates no filter and the request stays closed.
/// </summary>
/// <param name="Doc">The scope's own values as a JSON object in the value encoding.</param>
/// <param name="Parent">The parent document's values, for a field on a child row; null
/// for a field of the document itself.</param>
public sealed record LookupSnapshot(string Doc, string? Parent);

public sealed partial class DocumentDraft
{
    private readonly TimeProvider? _timeProvider;
    private RuleEngine? _rules;

    /// <summary>
    /// The instant field rules of this draft (ADR-28): the field's <c>depends_on</c>,
    /// <c>mandatory_depends_on</c> and <c>read_only_depends_on</c> evaluated locally
    /// through the shared expression language, and the <c>default_expr</c> values a new
    /// document starts with. The draft re-evaluates only the affected rules on every
    /// change and reports the batch through <see cref="RuleEngine.RulesChanged"/>; the
    /// renderer reads <see cref="FieldState"/> and nothing else.
    /// </summary>
    public RuleEngine Rules => _rules ??= new RuleEngine(this, _timeProvider);

    /// <summary>
    /// The lookup request's <c>doc</c> snapshot for a field of the document itself
    /// (<c>parent</c> is absent). Nothing here filters: the server owns the field's
    /// filter (ADR-068 §3, A3.2).
    /// </summary>
    public LookupSnapshot LookupSnapshot() => new(WriteSnapshot(Model, Get), null);

    /// <summary>
    /// The lookup request's snapshots for a field on child row <paramref name="row"/>
    /// (1-based, as every field path counts rows): <c>doc</c> is the row, <c>parent</c>
    /// the document.
    /// </summary>
    /// <param name="table">The table field's name.</param>
    /// <param name="row">The row, counted from 1.</param>
    public LookupSnapshot LookupSnapshot(string table, int row)
    {
        var draft = Table(table);
        if (row < 1 || row > draft.Rows.Count)
        {
            throw new ArgumentOutOfRangeException(nameof(row), row, $"The table {table} has {draft.Rows.Count} row(s).");
        }

        var rowDraft = draft.Rows[row - 1];
        var doc = draft.RowModel is { } rowModel
            ? WriteSnapshot(rowModel, rowDraft.GetValue)
            : WriteRowSnapshot(rowDraft);
        return new LookupSnapshot(doc, WriteSnapshot(Model, Get));
    }

    /// <summary>The document's (or a row model's) scalar values, in field order.</summary>
    private static string WriteSnapshot(DocTypeModel model, Func<string, FieldValue?> read)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            foreach (var field in model.Fields)
            {
                if (field.Kind == FieldKind.Table)
                {
                    // A child table is not a value a lookup's filter reads.
                    continue;
                }

                if (read(field.Name) is { } value)
                {
                    ValueCodec.WriteScalar(writer, field.Name, value);
                }
            }

            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>A row whose row DocType is not loaded yet: whatever the row carries.</summary>
    private static string WriteRowSnapshot(TableDraft.RowDraft row)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            foreach (var pair in row.Values)
            {
                if (pair.Value.IsMasked)
                {
                    // A masked value never leaves the client.
                    continue;
                }

                ValueCodec.WriteScalar(writer, pair.Key, pair.Value.Value);
            }

            writer.WriteEndObject();
        }

        return System.Text.Encoding.UTF8.GetString(stream.ToArray());
    }
}
