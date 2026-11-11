using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists.Tests;

public static class Fixtures
{
    public static DocTypeModel Model() => new("order", "Order", 1, false,
    [
        new FieldModel("title", "Title", FieldKind.Text, hints: new FieldHintsModel(InListView: true, InStandardFilter: true)),
        new FieldModel("customer", "Customer", FieldKind.Link, linkTo: "party", hints: new FieldHintsModel(InListView: true)),
        new FieldModel("amount", "Amount", FieldKind.DecimalNumber, scale: 2, precision: 18, hints: new FieldHintsModel(InListView: true, InStandardFilter: true)),
        new FieldModel("status", "Status", FieldKind.Select, options: ["Open", "Paid", "Void"], hints: new FieldHintsModel(InListView: true)),
        new FieldModel("due", "Due", FieldKind.Date),
        new FieldModel("qty", "Qty", FieldKind.IntegerNumber),
        new FieldModel("paid", "Paid", FieldKind.Bool),
        new FieldModel("stamp", "Stamp", FieldKind.Datetime),
        new FieldModel("internal_note", "Internal note", FieldKind.Text),
        new FieldModel("salary", "Salary", FieldKind.DecimalNumber, scale: 2, precision: 18, hints: new FieldHintsModel(InListView: true)),
        new FieldModel("notes", "Notes", FieldKind.LongText),
        new FieldModel("items", "Items", FieldKind.Table, childDoctype: "line"),
    ]);

    public static PermissionSnapshot Permissions(string fields = """{"hidden":["internal_note"],"masked":["salary"]}""", bool create = true)
        => PermissionSnapshot.Decode(
            $$$"""{"etag":"e1","data":{"doctypes":[{"doctype":"order","rights":{"create":{{{(create ? "true" : "false")}}}},"fields":{{{fields}}}}]}}""");

    public static PermissionReply Reply(DocumentRights rights, string fields = """{"hidden":["internal_note"],"masked":["salary"]}""", bool create = true)
        => new(Permissions(fields, create), rights);

    public static ListViewModelOptions Options(IListExportSink? sink = null) => new()
    {
        Culture = System.Globalization.CultureInfo.InvariantCulture,
        Zone = TimeZoneInfo.Utc,
        SearchDebounce = TimeSpan.Zero,
        Delay = (_, _) => Task.CompletedTask,
        ExportSink = sink,
        Source = new ListSourceOptions { PageSize = 100 },
    };

    public static ApiError Error(string code, string message, int status = 409, params ApiFieldError[] fields)
        => new() { Code = code, Message = message, StatusCode = status, FieldErrors = fields };
}

public sealed class FakeShell : IFormShell
{
    public List<string> Asked { get; } = [];
    public List<(string DocType, string? Id)> Opened { get; } = [];
    public List<Referrer> Referrers { get; } = [];
    public bool Confirm { get; set; } = true;
    public string? Text { get; set; } = "Mine";

    public Task<bool> ConfirmAsync(string title, string message, string confirmLabel)
    {
        Asked.Add("confirm: " + title + " | " + message);
        return Task.FromResult(Confirm);
    }

    public Task<string?> AskTextAsync(string title, string prompt, bool required)
    {
        Asked.Add("ask: " + title);
        return Task.FromResult(Text);
    }

    public Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed) => Task.FromResult(false);
    public Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info) => Task.FromResult(ConflictChoice.Keep);

    public Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers)
    {
        Asked.Add("referrers: " + title);
        Referrers.AddRange(referrers);
        return Task.CompletedTask;
    }

    public void OpenDocument(string doctype, string? id) => Opened.Add((doctype, id));
    public void CloseTab() { }
}

public sealed class CapturingSink : IListExportSink
{
    public List<(string Name, string Csv)> Saved { get; } = [];

    public Task SaveCsvAsync(string suggestedName, string csv)
    {
        Saved.Add((suggestedName, csv));
        return Task.CompletedTask;
    }
}

/// <summary>A server fake over N generated rows. Paging is by opaque cursor ("c:offset"); one query in flight
/// is asserted (MaxInFlight); latency honours cancellation; a hook lets a test hold a response.</summary>
public sealed class FakeListGateway : IListGateway
{
    private int _inFlight;

    public FakeListGateway(int rows = 1000, DocumentRights? rights = null)
    {
        Rows = rows;
        Rights = rights ?? DocumentRights.All;
    }

    public int Rows { get; set; }
    public int FilteredRows { get; set; } = 250;
    public DocumentRights Rights { get; set; }
    public string PermissionFields { get; set; } = """{"hidden":["internal_note"],"masked":["salary"]}""";
    public bool CanCreate { get; set; } = true;
    public TimeSpan Latency { get; set; }
    public int MaxInFlight { get; private set; }
    public List<string> Requests { get; } = [];
    public List<(string Target, string[] Ids)> LabelCalls { get; } = [];
    public List<(string Id, int Version)> Deleted { get; } = [];
    public Dictionary<string, ApiError> DeleteErrors { get; } = [];
    public Func<int, string, CancellationToken, Task>? Hold { get; set; }
    public Exception? FailNext { get; set; }
    public Exception? MetaFailure { get; set; }
    public bool BulkExport { get; set; }
    public string ExportCsv { get; set; } = "a,b\r\n1,2\r\n";
    public bool SupportsBulkExport => BulkExport;
    public string Prefix { get; set; } = "";

    public Task<DocTypeModel> GetMetaAsync(string doctype, CancellationToken cancellationToken)
        => MetaFailure is null ? Task.FromResult(Fixtures.Model()) : Task.FromException<DocTypeModel>(MetaFailure);

    public Task<PermissionReply> GetPermissionsAsync(string doctype, CancellationToken cancellationToken)
        => Task.FromResult(Fixtures.Reply(Rights, PermissionFields, CanCreate));

    public async Task<ListPage> QueryAsync(string doctype, string requestJson, CancellationToken cancellationToken)
    {
        var index = Requests.Count;
        Requests.Add(requestJson);
        MaxInFlight = Math.Max(MaxInFlight, Interlocked.Increment(ref _inFlight));
        try
        {
            if (FailNext is { } failure)
            {
                FailNext = null;
                throw failure;
            }

            if (Hold is not null)
            {
                await Hold(index, requestJson, cancellationToken).ConfigureAwait(true);
            }

            if (Latency > TimeSpan.Zero)
            {
                await Task.Delay(Latency, cancellationToken).ConfigureAwait(true);
            }

            cancellationToken.ThrowIfCancellationRequested();
            using var document = JsonDocument.Parse(requestJson);
            var root = document.RootElement;
            var limit = root.GetProperty("limit").GetInt32();
            var offset = root.TryGetProperty("cursor", out var cursor) ? int.Parse(cursor.GetString()![2..], System.Globalization.CultureInfo.InvariantCulture) : 0;
            var total = root.TryGetProperty("filter", out _) ? FilteredRows : Rows;
            var fields = root.GetProperty("fields").EnumerateArray().Select(f => f.GetString()!).ToArray();
            var records = new List<ListRecord>();
            for (var i = offset; i < Math.Min(total, offset + limit); i++)
            {
                records.Add(Record(i, fields));
            }

            var next = offset + limit < total ? "c:" + (offset + limit).ToString(System.Globalization.CultureInfo.InvariantCulture) : null;
            return new ListPage(records, next, root.TryGetProperty("with_total", out _) ? total : null);
        }
        finally
        {
            Interlocked.Decrement(ref _inFlight);
        }
    }

    public ListRecord Record(int i, IEnumerable<string> fields)
    {
        var values = new Dictionary<string, JsonElement>(StringComparer.Ordinal);
        foreach (var field in fields)
        {
            var json = field switch
            {
                "title" => $"\"{Prefix}Order {i}\"",
                "customer" => $"\"p-{i % 7}\"",
                "amount" => $"\"{i * 1000 + 5}.5\"",
                "status" => "\"Open\"",
                "due" => "\"2026-10-23\"",
                "qty" => i.ToString(System.Globalization.CultureInfo.InvariantCulture),
                "paid" => i % 2 == 0 ? "true" : "false",
                "salary" => "\"****\"",
                "internal_note" => "\"SECRET\"",
                _ => "null",
            };
            values[field] = JsonDocument.Parse(json).RootElement.Clone();
        }

        return new ListRecord("d-" + i.ToString("D6", System.Globalization.CultureInfo.InvariantCulture), i + 1, values);
    }

    public Task<IReadOnlyDictionary<string, string>> ResolveLabelsAsync(string targetDoctype, IReadOnlyCollection<string> ids, CancellationToken cancellationToken)
    {
        LabelCalls.Add((targetDoctype, ids.ToArray()));
        return Task.FromResult<IReadOnlyDictionary<string, string>>(ids.ToDictionary(id => id, id => "Party " + id));
    }

    public Task DeleteAsync(string doctype, string id, int version, CancellationToken cancellationToken)
    {
        if (DeleteErrors.TryGetValue(id, out var error))
        {
            throw new ApiRequestException(error);
        }

        Deleted.Add((id, version));
        return Task.CompletedTask;
    }

    public Task<string> ExportCsvAsync(string doctype, IReadOnlyList<string> columns, ListFilter? filter, IReadOnlyList<ListOrder> order, int maxRows, CancellationToken cancellationToken)
        => Task.FromResult(ExportCsv);
}
