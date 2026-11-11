using System.Collections.ObjectModel;
using System.Text.Json;

namespace Spotledger.Client.Metadata;

public sealed partial class DocumentDraft
{
    private PermissionSnapshot? _permissions;
    private readonly Dictionary<string, CurrencyInfo> _currency = new(StringComparer.Ordinal);

    /// <summary>The current permission snapshot version.</summary>
    public string? PermissionETag => _permissions?.ETag;

    /// <summary>Server currency presentation keyed by exact field path, including child-row paths.</summary>
    public IReadOnlyDictionary<string, CurrencyInfo> Currency => new ReadOnlyDictionary<string, CurrencyInfo>(_currency);

    /// <summary>Applies a newly fetched permission snapshot. Apply before loading documents;
    /// changed snapshots scrub values newly masked and revoke all previous reveals. Refetch
    /// the document after permission changes to obtain fresh masked text or newly readable values.</summary>
    public void ApplyPermissions(PermissionSnapshot summary)
    {
        ArgumentNullException.ThrowIfNull(summary);
        if (_permissions is not null && summary.ETag is not null && summary.ETag == _permissions.ETag)
        {
            return;
        }

        _permissions = summary;
        _suppressEvents++;
        try
        {
            foreach (var name in UnknownProperties.Keys.Where(IsRestrictedUnknown).ToArray())
            {
                UnknownProperties.Remove(name);
            }
            foreach (var field in Model.Fields)
            {
                ApplyFieldPermissions(Model.Name, field, _fields[field.Name]);
            }
            foreach (var table in _tables.Values)
            {
                if (_fields[table.Field.Name].IsMasked)
                {
                    table.LoadRows(new TableFieldValue(Array.Empty<TableRowValue>(), table.RowModel));
                }
                // A changed snapshot ends every undo: the journal holds pre-change values
                // (possibly now-masked cleartext) and an undo must never write them back.
                table.ClearJournal();
                table.RefreshPermissions();
                // The table aggregate must not retain pre-revocation row values.
                _fields[table.Field.Name].ReplaceLoadedValue(table.CurrentValue());
            }
            _currency.Clear();
            Rules.EvaluateAll();
        }
        finally
        {
            _suppressEvents--;
        }

        OnStructureChanged(new StructureChangedEventArgs(StructureChangeKind.PermissionsApplied));
    }

    private void ApplyFieldPermissions(string doctype, FieldModel field, FieldState state, string? path = null)
    {
        var permissions = _permissions is not null && _permissions.DocTypes.TryGetValue(doctype, out var found) ? found : null;
        var root = path is not null && _permissions?.DocTypes.TryGetValue(Model.Name, out var parent) == true ? parent : null;
        var masked = permissions?.Masked.Contains(field.Name, StringComparer.Ordinal) == true
            || (path is not null && root?.Masked.Contains(path, StringComparer.Ordinal) == true);
        var hidden = permissions?.Hidden.Contains(field.Name, StringComparer.Ordinal) == true
            || (path is not null && root?.Hidden.Contains(path, StringComparer.Ordinal) == true);
        var create = field.Kind == FieldKind.Link && field.LinkTo is not null
            && _permissions?.DocTypes.TryGetValue(field.LinkTo, out var target) == true && target.CanCreate;
        var limit = field.Kind == FieldKind.Attach && permissions?.AttachLimits.TryGetValue(field.Name, out var attachment) == true
            ? attachment : null;
        if (field.Kind == FieldKind.Attach && path is not null && root?.AttachLimits.TryGetValue(path, out var pathLimit) == true)
        {
            limit = pathLimit;
        }
        state.ApplyPermissions(masked || hidden, !hidden && (permissions?.Revealable.Contains(field.Name, StringComparer.Ordinal) == true
                || (path is not null && root?.Revealable.Contains(path, StringComparer.Ordinal) == true)),
            limit, create, !field.ReadOnly && field.Kind != FieldKind.Unknown
                && permissions?.ReadOnly.Contains(field.Name, StringComparer.Ordinal) != true && !hidden
                && (path is null || root?.ReadOnly.Contains(path, StringComparer.Ordinal) != true));
    }

    private void ConfigureRow(TableDraft table, TableDraft.RowDraft row, int ordinal)
    {
        foreach (var field in table.RowModel?.Fields ?? Array.Empty<FieldModel>())
        {
            row.EnsureField(field.Name);
            var state = row.Values[field.Name];
            // The row's states start from the field's static properties, exactly as the
            // document's do; the rule engine then applies the row's rules over them.
            state.IsVisible = !field.Hints.Hidden;
            state.IsMandatory = field.Required;
            state.IsReadOnly = field.ReadOnly;
            ApplyFieldPermissions(table.RowModel!.Name, field, state, FieldPath.OfRow(table.Field.Name, ordinal, field.Name));
        }
        foreach (var pair in row.Values)
        {
            if (table.RowModel?.Field(pair.Key) is not null) { continue; }
            ApplyFieldPermissions(table.RowModel?.Name ?? table.Field.ChildDoctype ?? string.Empty,
                new FieldModel(pair.Key, pair.Key, FieldKind.Unknown), pair.Value, FieldPath.OfRow(table.Field.Name, ordinal, pair.Key));
        }
    }

    private bool IsRestrictedUnknown(string name) => _permissions?.DocTypes.TryGetValue(Model.Name, out var permissions) == true
        && (permissions.Masked.Contains(name, StringComparer.Ordinal) || permissions.Hidden.Contains(name, StringComparer.Ordinal));

    private bool IsRowMasked(TableDraft table, int ordinal, string field)
    {
        var path = FieldPath.OfRow(table.Field.Name, ordinal, field);
        return (table.RowModel is not null && _permissions?.DocTypes.TryGetValue(table.RowModel.Name, out var permissions) == true
                && (permissions.Masked.Contains(field, StringComparer.Ordinal) || permissions.Hidden.Contains(field, StringComparer.Ordinal)))
            || (_permissions?.DocTypes.TryGetValue(Model.Name, out var root) == true
                && (root.Masked.Contains(path, StringComparer.Ordinal) || root.Hidden.Contains(path, StringComparer.Ordinal)));
    }

    private void LoadCurrency(JsonElement document)
    {
        _currency.Clear();
        if (!document.TryGetProperty("_currency", out var currency) || currency.ValueKind != JsonValueKind.Object)
        {
            return;
        }
        foreach (var entry in currency.EnumerateObject())
        {
            var state = FindState(entry.Name);
            if (state?.IsMasked == true)
            {
                continue;
            }
            _currency[entry.Name] = new(entry.Value.GetProperty("code").GetString()!, entry.Value.GetProperty("symbol").GetString()!);
        }
    }

    /// <summary>Loads the document body returned by a successful save, refreshing currency,
    /// masked text, version and values exactly as on a read. A value-only MarkSaved cannot
    /// invent fresh server presentation; app save handlers must pass the response here.</summary>
    public void LoadSavedFrom(string documentJson) => LoadFrom(documentJson);

    /// <summary>Decodes field_reveal's response into a separate read-only slot. No HTTP call
    /// or authorization is performed here; the app supplies only a successful reveal response.</summary>
    public void ApplyRevealResponse(string fieldPath, string responseJson)
    {
        var state = FindState(fieldPath) ?? throw new KeyNotFoundException($"Unknown field path {fieldPath}.");
        var model = FindModel(fieldPath) ?? throw new KeyNotFoundException($"Unknown field metadata {fieldPath}.");
        using var response = JsonDocument.Parse(responseJson);
        state.SetRevealed(ValueCodec.DecodeElement(response.RootElement.GetProperty("data").GetProperty("value"), model));
        OnFieldChanged(fieldPath);
    }

    private FieldState? FindState(string path)
    {
        if (_fields.TryGetValue(path, out var state))
        {
            return state;
        }
        return TryRowPath(path, out var table, out var row, out var field) && table.Rows[row].Values.TryGetValue(field, out state)
            ? state : null;
    }

    private FieldModel? FindModel(string path) => Model.Field(path)
        ?? (TryRowPath(path, out var table, out _, out var field) ? table.RowModel?.Field(field) : null);

    private bool TryRowPath(string path, out TableDraft table, out int row, out string field)
    {
        table = null!;
        row = -1;
        field = string.Empty;
        var open = path.IndexOf('[');
        var close = path.IndexOf("].", StringComparison.Ordinal);
        if (open <= 0 || close <= open || !_tables.TryGetValue(path[..open], out var found)
            || !int.TryParse(path.AsSpan(open + 1, close - open - 1), out var ordinal)
            || ordinal < 1 || ordinal > found.Rows.Count)
        {
            return false;
        }
        table = found;
        row = ordinal - 1;
        field = path[(close + 2)..];
        return true;
    }
}
