using System.Security.Cryptography;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Workspace;

/// <summary>
/// Turns a live <see cref="DocumentDraft"/> into a <see cref="DraftRecord"/> and back. This is the one
/// place that decides what may reach the disk: a field the server reported as masked or hidden for
/// this user, a hidden field and a secret-kind field are never captured, in the document or in a
/// child row; and on restore they are never written back.
/// </summary>
public static class DraftSnapshot
{
    /// <summary>
    /// Captures the draft's changes. A draft with no changes gives an empty record (which the
    /// coordinator treats as "nothing to keep").
    /// </summary>
    /// <param name="draft">The live draft.</param>
    /// <param name="key">Whose draft and which document.</param>
    /// <param name="savedAtUtc">The time to stamp.</param>
    /// <param name="title">The title for the recovery list.</param>
    /// <param name="tabState">The form's tab state.</param>
    public static DraftRecord Capture(DocumentDraft draft, DraftKey key, DateTimeOffset savedAtUtc, string? title = null, string? tabState = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        ArgumentNullException.ThrowIfNull(key);
        var fields = new List<FieldEdit>();
        var tables = new List<TableEdit>();
        foreach (var field in draft.Model.Fields)
        {
            var state = draft.Fields[field.Name];
            if (!MayPersist(field, state))
            {
                continue;
            }

            if (field.Kind == FieldKind.Table)
            {
                if (draft.Tables.TryGetValue(field.Name, out var table) && table.IsDirty)
                {
                    tables.Add(CaptureTable(field, state, table));
                }

                continue;
            }

            if (state.IsDirty)
            {
                fields.Add(new FieldEdit(field.Name, Encode(state.Value), Encode(state.Original)));
            }
        }

        return new DraftRecord
        {
            Key = key,
            BaseVersion = draft.Version,
            SavedAtUtc = savedAtUtc,
            Title = title,
            TabState = tabState,
            Fields = fields,
            Tables = tables,
        };
    }

    /// <summary>True when a draft may write into the field on restore: persistable, and editable now.</summary>
    internal static bool MayRestore(FieldModel field, FieldState state)
        => MayPersist(field, state) && !state.IsReadOnly && state.IsEditable;

    /// <summary>True when a field may be written to disk: not masked, not hidden, not a secret.</summary>
    internal static bool MayPersist(FieldModel field, FieldState state)
        => field.Kind != FieldKind.Secret
           && field.Hints.Hidden != true
           && state is { IsMasked: false, IsVisible: true };

    private static TableEdit CaptureTable(FieldModel field, FieldState tableState, DocumentDraft.TableDraft table)
    {
        var rows = new List<RowEdit>();
        foreach (var row in table.Rows)
        {
            var values = new Dictionary<string, string>(StringComparer.Ordinal);
            foreach (var (name, state) in row.Values)
            {
                var cell = table.RowModel?.Field(name);
                if (state.IsMasked || state.Value is SecretFieldValue || state.Value is TableFieldValue
                    || cell is { Kind: FieldKind.Secret } || cell?.Hints.Hidden == true)
                {
                    continue;
                }

                values[name] = Encode(state.Value);
            }

            rows.Add(new RowEdit(row.Id, values));
        }

        return new TableEdit(field.Name, rows, tableState.Original is TableFieldValue original ? HashTable(original) : string.Empty);
    }

    /// <summary>A hash of a loaded table (ids and cell values), to tell whether the server's table moved on.</summary>
    internal static string HashTable(TableFieldValue table)
    {
        var builder = new StringBuilder();
        foreach (var row in table.Rows)
        {
            builder.Append(row.Id ?? "-").Append('{');
            foreach (var pair in row.Values.OrderBy(p => p.Key, StringComparer.Ordinal))
            {
                if (pair.Value is SecretFieldValue or TableFieldValue)
                {
                    continue;
                }

                builder.Append(pair.Key).Append('=').Append(Encode(pair.Value)).Append(';');
            }

            builder.Append('}');
        }

        return Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(builder.ToString())));
    }

    /// <summary>A field value in the contract's JSON encoding.</summary>
    internal static string Encode(FieldValue value)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            ValueCodec.WriteValue(writer, value);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    internal static FieldValue Decode(string json, FieldModel field)
    {
        using var document = JsonDocument.Parse(json);
        return ValueCodec.DecodeElement(document.RootElement, field);
    }

    /// <summary>
    /// Works out what restoring <paramref name="record"/> into <paramref name="draft"/> would do, without
    /// touching the draft. A restore is an ordinary edit, so it obeys the edit gate: nothing is restored into a
    /// read-only or non-editable field, or into a document that is read-only (<paramref name="documentReadOnly"/>:
    /// no write right, posted or cancelled). When the document's version moved on, every field the draft edits whose value
    /// on the server is no longer the base value is a conflict, shown with both sides.
    /// </summary>
    public static RestorePlan Plan(DraftRecord record, DocumentDraft draft, bool documentReadOnly = false)
    {
        ArgumentNullException.ThrowIfNull(record);
        ArgumentNullException.ThrowIfNull(draft);
        var items = new List<RestoreItem>();
        var serverMoved = record.BaseVersion != draft.Version;
        foreach (var edit in record.Fields)
        {
            var field = draft.Model.Field(edit.Name);
            if (field is null || field.Kind == FieldKind.Table || !draft.Fields.TryGetValue(edit.Name, out var state))
            {
                items.Add(new RestoreItem(edit.Name, edit.Name, RestoreItemKind.Field, SkipReason: "The field no longer exists."));
                continue;
            }

            if (documentReadOnly || !MayRestore(field, state))
            {
                items.Add(new RestoreItem(edit.Name, field.Label, RestoreItemKind.Field, SkipReason: documentReadOnly ? "This document is read-only." : "You can no longer see or change this field."));
                continue;
            }

            var server = Encode(state.Original);
            var changedOnServer = serverMoved && !string.Equals(server, edit.BaseJson, StringComparison.Ordinal);
            items.Add(new RestoreItem(edit.Name, field.Label, RestoreItemKind.Field, YourValue: Display(edit.Json), ServerValue: Display(server), IsConflict: changedOnServer));
        }

        foreach (var edit in record.Tables)
        {
            var field = draft.Model.Field(edit.Name);
            if (field is null || field.Kind != FieldKind.Table || !draft.Tables.ContainsKey(edit.Name) || !draft.Fields.TryGetValue(edit.Name, out var state))
            {
                items.Add(new RestoreItem(edit.Name, edit.Name, RestoreItemKind.Table, SkipReason: "The table no longer exists."));
                continue;
            }

            if (documentReadOnly || !MayRestore(field, state))
            {
                items.Add(new RestoreItem(edit.Name, field.Label, RestoreItemKind.Table, SkipReason: documentReadOnly ? "This document is read-only." : "You can no longer see or change this table."));
                continue;
            }

            var serverHash = state.Original is TableFieldValue original ? HashTable(original) : string.Empty;
            var changedOnServer = serverMoved && edit.BaseHash.Length > 0 && serverHash.Length > 0
                                  && !string.Equals(serverHash, edit.BaseHash, StringComparison.Ordinal);
            var serverRows = state.Original is TableFieldValue t ? t.Rows.Count : 0;
            items.Add(new RestoreItem(edit.Name, field.Label, RestoreItemKind.Table,
                YourValue: edit.Rows.Count + " rows", ServerValue: serverRows + " rows", IsConflict: changedOnServer));
        }

        return new RestorePlan(record, serverMoved, draft.Version, items);
    }

    private static string Display(string json)
    {
        try
        {
            using var document = JsonDocument.Parse(json);
            return document.RootElement.ValueKind switch
            {
                JsonValueKind.String => document.RootElement.GetString() ?? string.Empty,
                JsonValueKind.Null => string.Empty,
                _ => document.RootElement.GetRawText(),
            };
        }
        catch (JsonException)
        {
            return json;
        }
    }

    /// <summary>
    /// Applies the plan to the draft as ordinary edits (the form shows them as unsaved changes the user
    /// reviews and saves; nothing is sent). Conflicting items are applied only when
    /// <paramref name="includeConflicts"/> is true. Returns the paths that were applied.
    /// </summary>
    public static IReadOnlyList<string> Apply(RestorePlan plan, DocumentDraft draft, bool includeConflicts)
    {
        ArgumentNullException.ThrowIfNull(plan);
        ArgumentNullException.ThrowIfNull(draft);
        var applied = new List<string>();
        foreach (var item in plan.Items)
        {
            if (item.SkipReason is not null || (item.IsConflict && !includeConflicts))
            {
                continue;
            }

            if (item.Kind == RestoreItemKind.Field)
            {
                var edit = plan.Record.Fields.First(f => f.Name == item.Name);
                var field = draft.Model.Field(item.Name)!;
                draft.Set(item.Name, Decode(edit.Json, field));
                applied.Add(item.Name);
                continue;
            }

            ApplyTable(plan.Record.Tables.First(t => t.Name == item.Name), draft);
            applied.Add(item.Name);
        }

        return applied;
    }

    private static void ApplyTable(TableEdit edit, DocumentDraft draft)
    {
        var table = draft.Tables[edit.Name];
        using var operation = table.Begin("restore " + edit.Name);
        for (var i = table.Rows.Count - 1; i >= 0; i--)
        {
            table.RemoveRowAt(i);
        }

        foreach (var saved in edit.Rows)
        {
            var row = table.AddRow();
            foreach (var (name, json) in saved.Values)
            {
                var cell = table.RowModel?.Field(name);
                if (cell is null || cell.Kind == FieldKind.Secret || cell.Kind == FieldKind.Table)
                {
                    continue;
                }

                if (row.Values.TryGetValue(name, out var existing) && (existing.IsMasked || existing.IsReadOnly || !existing.IsEditable))
                {
                    continue;
                }

                row.SetValue(name, Decode(json, cell));
            }

            if (saved.Id is { Length: > 0 } id)
            {
                table.AssignRowId(row, id);
            }
        }

        operation.Commit();
    }
}

/// <summary>What a restore would do for one field or table.</summary>
/// <param name="Name">The field's storage name.</param>
/// <param name="Label">The field's label.</param>
/// <param name="Kind">A field or a child table.</param>
/// <param name="YourValue">What the draft holds.</param>
/// <param name="ServerValue">What the server holds now.</param>
/// <param name="IsConflict">True when the server changed this field since the draft began.</param>
/// <param name="SkipReason">Why the item cannot be restored (null when it can).</param>
public sealed record RestoreItem(
    string Name,
    string Label,
    RestoreItemKind Kind,
    string? YourValue = null,
    string? ServerValue = null,
    bool IsConflict = false,
    string? SkipReason = null);

/// <summary>Whether a restore item is a scalar field or a child table.</summary>
public enum RestoreItemKind
{
    /// <summary>A scalar field.</summary>
    Field,

    /// <summary>A child table.</summary>
    Table,
}

/// <summary>The outcome of comparing a stored draft with the document as loaded now.</summary>
/// <param name="Record">The stored draft.</param>
/// <param name="ServerVersionChanged">True when the document's version is not the draft's base version.</param>
/// <param name="ServerVersion">The version the document has now.</param>
/// <param name="Items">One entry per changed field or table.</param>
public sealed record RestorePlan(DraftRecord Record, bool ServerVersionChanged, int ServerVersion, IReadOnlyList<RestoreItem> Items)
{
    /// <summary>The items the server changed since the draft began.</summary>
    public IReadOnlyList<RestoreItem> Conflicts => [.. Items.Where(i => i.IsConflict)];

    /// <summary>True when restoring needs the user to look first (the server's version moved on).</summary>
    public bool NeedsReview => ServerVersionChanged;
}
