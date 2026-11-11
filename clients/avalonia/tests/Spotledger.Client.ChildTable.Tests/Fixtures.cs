using System.Globalization;
using System.Text;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.ChildTable.Tests;

/// <summary>An invoice with one child table of eight columns: item (link), desc, qty, rate,
/// amount (read-only), unit (select), due (date), taxable (bool).</summary>
public static class Fixtures
{
    public const int Item = 0;
    public const int Desc = 1;
    public const int Qty = 2;
    public const int Rate = 3;
    public const int Amount = 4;
    public const int Unit = 5;
    public const int Due = 6;
    public const int Taxable = 7;

    public static DocTypeModel Model()
    {
        var line = new DocTypeModel("line", "Line", 1, false,
        [
            new FieldModel("item", "Item", FieldKind.Link, linkTo: "item"),
            new FieldModel("desc", "Description", FieldKind.Text),
            new FieldModel("qty", "Qty", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18),
            new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true),
            new FieldModel("unit", "Unit", FieldKind.Select, options: ["Nos", "Kg", "Box"]),
            new FieldModel("due", "Due", FieldKind.Date),
            new FieldModel("taxable", "Taxable", FieldKind.Bool),
        ]);
        return new DocTypeModel("invoice", "Invoice", 1, false,
            [new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line")],
            new Dictionary<string, DocTypeModel> { ["items"] = line });
    }

    /// <summary>A loaded draft with <paramref name="rows"/> rows, ids 1..n, qty = i, rate = 10.00.</summary>
    public static DocumentDraft Draft(int rows = 3, string? state = "draft", PermissionSnapshot? permissions = null)
    {
        var draft = new DocumentDraft(Model());
        if (permissions is not null)
        {
            draft.ApplyPermissions(permissions);
        }

        var json = new StringBuilder("{");
        if (state is not null)
        {
            json.Append(CultureInfo.InvariantCulture, $"\"_state\":\"{state}\",");
        }

        json.Append("\"items\":[");
        for (var i = 1; i <= rows; i++)
        {
            if (i > 1)
            {
                json.Append(',');
            }

            json.Append(CultureInfo.InvariantCulture,
                $"{{\"id\":\"r{i}\",\"item\":\"ITEM-{i}\",\"desc\":\"Line {i}\",\"qty\":\"{i}.00\",\"rate\":\"10.00\",\"amount\":\"{i * 10}.00\",\"unit\":\"Nos\",\"due\":\"2026-11-{(i % 28) + 1:00}\",\"taxable\":true}}");
        }

        json.Append("]}");
        draft.LoadFrom(json.ToString());
        return draft;
    }

    public static PermissionSnapshot Permissions(string line = "{}", string items = "{}")
        => PermissionSnapshot.Decode(
            $$$"""{"etag":"e1","data":{"doctypes":[{"doctype":"line","rights":{"create":true},"fields":{{{line}}}},{"doctype":"invoice","rights":{"create":true},"fields":{{{items}}}}]}}""");

    public static ChildTableModel Model(DocumentDraft draft, bool canWrite = true, ChildTableView? view = null)
        => new(draft, "items", view) { CanWrite = canWrite };

    public static string Text(this ChildTableModel model, int row, int column)
        => CellFormatter.Format(model.Columns[column].Field, model.Table.Rows[row].Values[model.Columns[column].Field.Name].Value);

    public static List<string?> Ids(this ChildTableModel model) => model.Table.Rows.Select(row => row.Id).ToList();
}

/// <summary>A lookup that answers from a fixed list and records what it was asked.</summary>
public sealed class FakeLookup(params string[] ids) : ILookupSource
{
    public List<string> Searches { get; } = [];

    public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken)
    {
        Searches.Add(text);
        IReadOnlyList<LookupItem> items = ids
            .Where(id => id.Contains(text, StringComparison.OrdinalIgnoreCase))
            .Select(id => new LookupItem(id, id + " name"))
            .ToArray();
        return Task.FromResult(items);
    }

    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken)
        => Task.FromResult<LookupItem?>(ids.Contains(id) ? new LookupItem(id, id + " name") : null);
}

public sealed class InstantDelay : ILookupDelay
{
    public Task WaitAsync(TimeSpan delay, CancellationToken token) => Task.CompletedTask;
}

public sealed class FakeClipboard(string? text) : IChildTableClipboard
{
    public string? Text { get; set; } = text;

    public Task<string?> GetTextAsync() => Task.FromResult(Text);
}

/// <summary>Drives the control through its real key pipeline.</summary>
public static class Keys
{
    public static Interactive Target(SpotChildTable table)
        => (table.Grid.IsEditing
            ? TopLevel.GetTopLevel(table)?.FocusManager?.GetFocusedElement() as Interactive
            : null) ?? table.Grid;

    /// <summary>Raises the key on the focused element and nothing else: no layout, no jobs.</summary>
    public static void Raise(SpotChildTable table, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => Target(table).RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });

    public static void Press(SpotChildTable table, Key key, KeyModifiers modifiers = KeyModifiers.None)
    {
        Raise(table, key, modifiers);
        Dispatcher.UIThread.RunJobs();
    }

    public static void Type(SpotChildTable table, string text)
    {
        var target = Target(table);
        target.RaiseEvent(new TextInputEventArgs { Text = text, RoutedEvent = InputElement.TextInputEvent });
    }

    public static SpotChildTable Show(DocumentDraft draft, bool canWrite = true, ChildTableView? view = null,
        Func<ChildTableLookupRequest, ILookupSource?>? lookups = null, double width = 900, double height = 500)
    {
        var table = new SpotChildTable(draft, "items", view, lookups, new InstantDelay()) { CanWrite = canWrite };
        TestHost.Show(table, width, height);
        TestHost.Pump();
        return table;
    }
}
