using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Effects.Tests.Toml;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects.Tests.Corpus;

/// <summary>
/// Renders a client <see cref="FormState"/> in the shape of the corpus's
/// <c>[case.expect.state]</c> snapshot: the values by wire path (a table as its row array,
/// row ids included), the read-only, hidden and required sets, the candidate values, labels,
/// filters, actions, messages, dialogs and focus. Comparing the rendered tree with the
/// checked-in snapshot is the conformance assertion.
/// </summary>
internal static class StateSnapshot
{
    /// <summary>Renders <paramref name="form"/> as a TOML-like tree.</summary>
    public static Dictionary<string, object?> Render(FormState form)
    {
        ArgumentNullException.ThrowIfNull(form);
        var state = new Dictionary<string, object?>(StringComparer.Ordinal);

        var values = new Dictionary<string, object?>(StringComparer.Ordinal);
        foreach (FieldModel field in form.Draft.Model.Fields)
        {
            if (field.Kind == FieldKind.Table)
            {
                DocumentDraft.TableDraft table = form.Draft.Table(field.Name);
                if (table.Rows.Count == 0)
                {
                    // The Rust snapshot omits an empty table (Document::tables skips it).
                    continue;
                }

                var rows = new List<object?>(table.Rows.Count);
                foreach (DocumentDraft.TableDraft.RowDraft row in table.Rows)
                {
                    var entry = new Dictionary<string, object?>(StringComparer.Ordinal);
                    if (row.Id is { } id)
                    {
                        entry["id"] = id;
                    }

                    foreach (FieldModel column in table.RowModel?.Fields ?? Array.Empty<FieldModel>())
                    {
                        if (Scalar(row.GetValue(column.Name)) is { } cell)
                        {
                            entry[column.Name] = cell;
                        }
                    }

                    rows.Add(entry);
                }

                values[field.Name] = rows;
                continue;
            }

            if (Scalar(form.Draft.Get(field.Name)) is { } value)
            {
                values[field.Name] = value;
            }
        }

        if (values.Count > 0)
        {
            state["values"] = values;
        }

        AddSet(state, "read_only", form.Presentation.ReadOnlyPaths);
        AddSet(state, "hidden", form.Presentation.HiddenPaths);
        AddSet(state, "required", form.Presentation.RequiredPaths);

        if (form.Presentation.Options.Count > 0)
        {
            var options = new Dictionary<string, object?>(StringComparer.Ordinal);
            foreach (KeyValuePair<string, IReadOnlyList<OptionItem>> pair in form.Presentation.Options)
            {
                var items = new List<object?>(pair.Value.Count);
                foreach (OptionItem option in pair.Value)
                {
                    var entry = new Dictionary<string, object?>(StringComparer.Ordinal) { ["value"] = option.Value };
                    if (option.Label is { } label)
                    {
                        entry["label"] = label;
                    }

                    items.Add(entry);
                }

                options[pair.Key] = items;
            }

            state["options"] = options;
        }

        if (form.Presentation.Labels.Count > 0)
        {
            var labels = new Dictionary<string, object?>(StringComparer.Ordinal);
            foreach (KeyValuePair<string, string> pair in form.Presentation.Labels)
            {
                labels[pair.Key] = pair.Value;
            }

            state["labels"] = labels;
        }

        if (form.Presentation.Filters.Count > 0)
        {
            var filters = new Dictionary<string, object?>(StringComparer.Ordinal);
            foreach (KeyValuePair<string, string> pair in form.Presentation.Filters)
            {
                filters[pair.Key] = pair.Value;
            }

            state["filters"] = filters;
        }

        if (form.Actions.Count > 0)
        {
            var actions = new List<object?>(form.Actions.Count);
            foreach (FormAction action in form.Actions)
            {
                actions.Add(new Dictionary<string, object?>(StringComparer.Ordinal)
                {
                    ["id"] = action.Id,
                    ["label"] = action.Label,
                });
            }

            state["actions"] = actions;
        }

        if (form.Messages.Count > 0)
        {
            var messages = new List<object?>(form.Messages.Count);
            foreach (FormMessage message in form.Messages)
            {
                var entry = new Dictionary<string, object?>(StringComparer.Ordinal)
                {
                    ["level"] = message.Level.ToWireName(),
                };
                if (message.Text is { } text)
                {
                    entry["text"] = text;
                }

                if (message.TextKey is { } key)
                {
                    entry["text_key"] = key;
                }

                if (message.Field is { } field)
                {
                    entry["field"] = field;
                }

                messages.Add(entry);
            }

            state["messages"] = messages;
        }

        if (form.Dialogs.Count > 0)
        {
            var dialogs = new List<object?>(form.Dialogs.Count);
            foreach (DialogIntent dialog in form.Dialogs)
            {
                var entry = new Dictionary<string, object?>(StringComparer.Ordinal) { ["dialog_id"] = dialog.DialogId };
                if (dialog.ParamsJson is { } parameters)
                {
                    entry["params"] = Parse(parameters);
                }

                dialogs.Add(entry);
            }

            state["dialogs"] = dialogs;
        }

        if (form.Focus is { } focus)
        {
            state["focus"] = focus;
        }

        return state;
    }

    /// <summary>Structural equality over the rendered tree and the parsed snapshot.</summary>
    public static bool Equal(object? left, object? right)
    {
        switch (left)
        {
            case null:
                return right is null;
            case string text:
                return right is string other && string.Equals(text, other, StringComparison.Ordinal);
            case long number:
                return right is long otherNumber && number == otherNumber;
            case bool flag:
                return right is bool otherFlag && flag == otherFlag;
            case IReadOnlyDictionary<string, object?> table:
                return right is IReadOnlyDictionary<string, object?> otherTable
                    && table.Count == otherTable.Count
                    && table.All(pair => otherTable.TryGetValue(pair.Key, out object? value) && Equal(pair.Value, value));
            case List<object?> list:
                if (right is not List<object?> otherList || list.Count != otherList.Count)
                {
                    return false;
                }

                for (int i = 0; i < list.Count; i++)
                {
                    if (!Equal(list[i], otherList[i]))
                    {
                        return false;
                    }
                }

                return true;
            default:
                return false;
        }
    }

    /// <summary>Renders a tree as JSON, for a failure message that can be diffed by eye.</summary>
    public static string Describe(object? node)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream, new JsonWriterOptions { Indented = true }))
        {
            TomlJson.WriteNodeTo(writer, node);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    /// <summary>
    /// The differences between two trees, path by path: a failure message that names the
    /// first divergence instead of printing two 40-line snapshots.
    /// </summary>
    public static IReadOnlyList<string> Differences(object? left, object? right, string path = "")
    {
        var differences = new List<string>();
        switch (left)
        {
            case IReadOnlyDictionary<string, object?> table when right is IReadOnlyDictionary<string, object?> other:
                foreach (string key in table.Keys.Union(other.Keys, StringComparer.Ordinal).OrderBy(k => k, StringComparer.Ordinal))
                {
                    table.TryGetValue(key, out object? a);
                    other.TryGetValue(key, out object? b);
                    if (!table.ContainsKey(key))
                    {
                        differences.Add(string.Create(CultureInfo.InvariantCulture, $"{path}.{key}: the client has nothing, the snapshot has {Describe(b)}"));
                        continue;
                    }

                    if (!other.ContainsKey(key))
                    {
                        differences.Add(string.Create(CultureInfo.InvariantCulture, $"{path}.{key}: the client has {Describe(a)}, the snapshot has nothing"));
                        continue;
                    }

                    differences.AddRange(Differences(a, b, string.Create(CultureInfo.InvariantCulture, $"{path}.{key}")));
                }

                return differences;
            case List<object?> list when right is List<object?> otherList:
                if (list.Count != otherList.Count)
                {
                    differences.Add(string.Create(
                        CultureInfo.InvariantCulture,
                        $"{path}: the client has {list.Count} entries, the snapshot has {otherList.Count}"));
                    return differences;
                }

                for (int i = 0; i < list.Count; i++)
                {
                    differences.AddRange(Differences(list[i], otherList[i], string.Create(CultureInfo.InvariantCulture, $"{path}[{i}]")));
                }

                return differences;
            default:
                if (!Equal(left, right))
                {
                    differences.Add(string.Create(
                        CultureInfo.InvariantCulture,
                        $"{(path.Length == 0 ? "<state>" : path)}: the client has {Describe(left)}, the snapshot has {Describe(right)}"));
                }

                return differences;
        }
    }

    private static void AddSet(Dictionary<string, object?> state, string name, IEnumerable<string> paths)
    {
        string[] sorted = paths.OrderBy(path => path, StringComparer.Ordinal).ToArray();
        if (sorted.Length == 0)
        {
            return;
        }

        state[name] = sorted.Cast<object?>().ToList();
    }

    private static object? Scalar(FieldValue? value) => value switch
    {
        null or NullFieldValue => null,
        BoolFieldValue flag => flag.Value,
        IntFieldValue number => number.Value,
        DecimalFieldValue exact => exact.CanonicalText,
        TextFieldValue text => text.Value,
        SelectFieldValue select => select.Value,
        MultiSelectFieldValue multiple => multiple.Values.Cast<object?>().ToList(),
        DateFieldValue date => date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
        TimeFieldValue time => time.Value.ToString("HH:mm:ss", CultureInfo.InvariantCulture),
        DateTimeFieldValue moment => moment.Value.ToString("O", CultureInfo.InvariantCulture),
        LinkFieldValue link => link.Id,
        AttachFieldValue attach => attach.FileId,
        SecretFieldValue secret => secret.IsSet,
        BytesFieldValue bytes => bytes.Base64,
        JsonFieldValue json => Parse(json.JsonText),
        _ => null,
    };

    private static object? Parse(string json)
    {
        using JsonDocument document = JsonDocument.Parse(json);
        return FromElement(document.RootElement);
    }

    private static object? FromElement(JsonElement element) => element.ValueKind switch
    {
        JsonValueKind.Object => element.EnumerateObject().ToDictionary(
            property => property.Name,
            property => FromElement(property.Value),
            StringComparer.Ordinal),
        JsonValueKind.Array => element.EnumerateArray().Select(FromElement).ToList(),
        JsonValueKind.String => element.GetString(),
        JsonValueKind.Number => element.TryGetInt64(out long number) ? number : element.GetRawText(),
        JsonValueKind.True => true,
        JsonValueKind.False => false,
        _ => null,
    };
}
