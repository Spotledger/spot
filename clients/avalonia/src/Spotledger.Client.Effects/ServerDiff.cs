using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects;

/// <summary>
/// Applies the document diff a form event's response carries: the shape
/// <c>resource_update</c> takes, where each top-level member replaces the value at that path
/// and a child table arrives as its full row array.
/// </summary>
/// <remarks>
/// <para>
/// A member the client's metadata does not know is ignored, never fatal: a newer server may
/// carry fields this client predates, and dropping the whole diff over one would lose the
/// rest of the round. A masked field is never overwritten by a diff (the client cannot show
/// the value anyway). Standard members (<c>id</c>, <c>version</c>, <c>_state</c>,
/// <c>_currency</c>) are not values and are skipped — the session keeps the version itself.
/// </para>
/// <para>
/// A table's diff replaces the row set in the draft, keeping the ids the server sent and
/// taking the array order as the new order (the contract's <c>idx</c>).
/// </para>
/// </remarks>
public static class ServerDiff
{
    private static readonly JsonDocumentOptions DocumentOptions = new()
    {
        AllowTrailingCommas = false,
        CommentHandling = JsonCommentHandling.Disallow,
    };

    /// <summary>The top-level members of a diff, which is what decides whether it overrides a path.</summary>
    /// <param name="diffJson">The diff body, or null for none.</param>
    public static IReadOnlyList<string> Members(string? diffJson)
    {
        var members = new List<string>();
        if (string.IsNullOrEmpty(diffJson))
        {
            return members;
        }

        try
        {
            using JsonDocument document = JsonDocument.Parse(diffJson, DocumentOptions);
            if (document.RootElement.ValueKind != JsonValueKind.Object)
            {
                return members;
            }

            foreach (JsonProperty property in document.RootElement.EnumerateObject())
            {
                members.Add(property.Name);
            }
        }
        catch (JsonException)
        {
            return members;
        }

        return members;
    }

    /// <summary>Whether <paramref name="path"/> is a path the diff replaces.</summary>
    /// <param name="members">The diff's top-level members (<see cref="Members"/>).</param>
    /// <param name="path">A wire field path.</param>
    public static bool Overrides(IReadOnlyList<string> members, string path)
    {
        ArgumentNullException.ThrowIfNull(members);
        if (members.Count == 0)
        {
            return false;
        }

        foreach (string member in members)
        {
            if (string.Equals(member, path, StringComparison.Ordinal))
            {
                return true;
            }
        }

        // A table diff replaces every row, so every row column path is overridden by the
        // table's own name.
        return EffectPath.TryParse(path, out EffectPath parsed)
            && parsed.IsRow
            && Contains(members, parsed.Table!);
    }

    /// <summary>
    /// Applies <paramref name="diffJson"/> to <paramref name="draft"/>, answering every member
    /// it could not apply. An empty or null body is a no-op.
    /// </summary>
    public static IReadOnlyList<EffectProblem> Apply(DocumentDraft draft, string? diffJson)
    {
        ArgumentNullException.ThrowIfNull(draft);
        var problems = new List<EffectProblem>();
        if (string.IsNullOrEmpty(diffJson))
        {
            return problems;
        }

        JsonDocument document;
        try
        {
            document = JsonDocument.Parse(diffJson, DocumentOptions);
        }
        catch (JsonException)
        {
            problems.Add(EffectProblem.Of(EffectCodes.EffectInvalid, "A document diff is valid JSON."));
            return problems;
        }

        using (document)
        {
            if (document.RootElement.ValueKind == JsonValueKind.Null)
            {
                return problems;
            }

            if (document.RootElement.ValueKind != JsonValueKind.Object)
            {
                problems.Add(EffectProblem.Of(EffectCodes.EffectInvalid, "A document diff is an object."));
                return problems;
            }

            foreach (JsonProperty property in document.RootElement.EnumerateObject())
            {
                ApplyMember(draft, property, problems);
            }
        }

        return problems;
    }

    private static void ApplyMember(DocumentDraft draft, JsonProperty property, List<EffectProblem> problems)
    {
        string name = property.Name;
        if (name.Length == 0 || name.StartsWith('_')
            || string.Equals(name, "id", StringComparison.Ordinal)
            || string.Equals(name, "version", StringComparison.Ordinal)
            || string.Equals(name, "idx", StringComparison.Ordinal))
        {
            return;
        }

        FieldModel? field = draft.Model.Field(name);
        if (field is null)
        {
            // A field this client's metadata predates: keep the rest of the diff.
            return;
        }

        if (draft.Fields.TryGetValue(name, out FieldState? state) && state.IsMasked)
        {
            return;
        }

        try
        {
            if (field.Kind == FieldKind.Table && draft.Tables.TryGetValue(name, out DocumentDraft.TableDraft? table))
            {
                if (property.Value.ValueKind == JsonValueKind.Null)
                {
                    table.LoadRows(new TableFieldValue(Array.Empty<TableRowValue>(), table.RowModel));
                    return;
                }

                if (property.Value.ValueKind != JsonValueKind.Array)
                {
                    problems.Add(EffectProblem.Of(
                        EffectCodes.EffectInvalid,
                        $"The diff's `{name}` is not a row array.").WithPath(name));
                    return;
                }

                table.LoadRows(ValueCodec.DecodeTable(property.Value, table.RowModel));
                return;
            }

            draft.Set(name, ValueCodec.DecodeElement(property.Value, field));
        }
        catch (InvalidOperationException error)
        {
            problems.Add(EffectProblem.Of(EffectCodes.EffectInvalid, error.Message).WithPath(name));
        }
        catch (FormatException error)
        {
            problems.Add(EffectProblem.Of(EffectCodes.EffectInvalid, error.Message).WithPath(name));
        }
    }

    private static bool Contains(IReadOnlyList<string> members, string name)
    {
        foreach (string member in members)
        {
            if (string.Equals(member, name, StringComparison.Ordinal))
            {
                return true;
            }
        }

        return false;
    }
}
