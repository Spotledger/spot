using System.Globalization;
using System.Text;
using Spotledger.Client.Api;
using Spotledger.Client.Reports;

namespace Spotledger.Client.Reports.Tests;

public static class Fx
{
    public const string AllTypesDescription = """
        {"data":{"name":"sales_by_customer","label":"Sales by customer","app":"selling","base_doctype":"sales_invoice","max_rows":50000,
         "parameters":[
           {"name":"customer","label":"Customer","type":"link","required":true,"options":{"source":"link","doctype":"customer"}},
           {"name":"from_date","label":"From date","type":"date","required":true,"default":"2026-01-01"},
           {"name":"to_date","label":"To date","type":"date","required":false},
           {"name":"min_amount","label":"Minimum amount","type":"decimal","required":false,"default":"100.50"},
           {"name":"min_lines","label":"Minimum lines","type":"int","required":false,"default":3},
           {"name":"status","label":"Status","type":"select","required":false,"default":"Open","options":{"source":"values","values":["Open","Paid","Overdue"]}},
           {"name":"note","label":"Note contains","type":"text","required":false}],
         "columns":[
           {"name":"account","label":"Account","type":"link"},
           {"name":"posted","label":"Posted","type":"date"},
           {"name":"memo","label":"Memo","type":"text"},
           {"name":"balance","label":"Balance","type":"decimal","summable":true}]}}
        """;

    public static ReportDescription Simple(int parameters = 2, int maxRows = 1000, bool summable = true)
    {
        var defs = new List<ReportParameterDef>();
        for (var i = 0; i < parameters; i++)
        {
            var name = "p" + i.ToString(CultureInfo.InvariantCulture);
            defs.Add((i % 4) switch
            {
                0 => new ReportParameterDef(name, "Param " + i, ReportParameterType.Text, i == 0, null, null),
                1 => new ReportParameterDef(name, "Param " + i, ReportParameterType.IntegerNumber, false, null, null),
                2 => new ReportParameterDef(name, "Param " + i, ReportParameterType.DecimalNumber, false, null, null),
                _ => new ReportParameterDef(name, "Param " + i, ReportParameterType.Select, false, null, new ReportParameterOptions(["A", "B"], null)),
            });
        }

        return new ReportDescription("aging", "Receivables aging", "accounts", "sales_invoice", defs,
            [new ReportColumn("party", "Party", "link"), new ReportColumn("memo", "Memo", "text"), new ReportColumn("balance", "Balance", "decimal", summable)],
            maxRows);
    }

    public static ReportRunResult Result(int rows = 3, bool truncated = false, string[]? balances = null, IReadOnlyList<ReportMessage>? messages = null, bool summable = false)
    {
        var columns = new[] { new ReportColumn("party", "Party", "link"), new ReportColumn("memo", "Memo", "text"), new ReportColumn("balance", "Balance", "decimal", summable) };
        var data = new List<string?[]>();
        for (var i = 0; i < rows; i++)
        {
            data.Add([$"C{i:D5}", $"Row {i}", balances is null ? $"{i}.50" : balances[i % balances.Length]]);
        }

        return new ReportRunResult(columns, data, truncated, messages ?? []);
    }

    public static ApiError Error(string code, string message, int status = 422, params ApiFieldError[] fields)
        => new() { Code = code, Message = message, StatusCode = status, FieldErrors = fields };

    public static ReportViewModel Model(FakeReportGateway gateway, FakeReportShell? shell = null, IReportSettingsStore? store = null, ReportViewModelOptions? options = null)
        => new(gateway, shell ?? new FakeReportShell(), store ?? new InMemoryReportSettingsStore(), "aging", "ana", options);
}

public sealed class FakeReportGateway : IReportGateway
{
    public List<ReportSummary> Reports { get; } = [];

    public ReportDescription Description { get; set; } = Fx.Simple();

    public Func<string, int, CancellationToken, Task<ReportRunResult>>? OnRun { get; set; }

    public ReportRunResult Result { get; set; } = Fx.Result();

    public ApiRequestException? FailList { get; set; }

    public ApiRequestException? FailDescribe { get; set; }

    public ApiRequestException? FailRun { get; set; }

    public bool ServerExport { get; set; }

    public int ServerExports { get; private set; }

    public int Runs { get; private set; }

    public int InFlight { get; private set; }

    public int MaxInFlight { get; private set; }

    public List<(string Report, string Filters, int Limit)> Calls { get; } = [];

    public Task<IReadOnlyList<ReportSummary>> ListAsync(CancellationToken cancellationToken)
        => FailList is { } error ? Task.FromException<IReadOnlyList<ReportSummary>>(error) : Task.FromResult<IReadOnlyList<ReportSummary>>(Reports.ToArray());

    public Task<ReportDescription>? HoldDescribe { get; set; }

    public async Task<ReportDescription> DescribeAsync(string report, CancellationToken cancellationToken)
    {
        if (HoldDescribe is { } hold)
        {
            await hold.WaitAsync(cancellationToken).ConfigureAwait(false);
        }

        return FailDescribe is { } error ? throw error : Description;
    }

    public async Task<ReportRunResult> RunAsync(string report, string filtersJson, int limit, CancellationToken cancellationToken)
    {
        Runs++;
        Calls.Add((report, filtersJson, limit));
        InFlight++;
        MaxInFlight = Math.Max(MaxInFlight, InFlight);
        try
        {
            if (FailRun is { } error)
            {
                throw error;
            }

            return OnRun is null ? Result : await OnRun(filtersJson, limit, cancellationToken).ConfigureAwait(false);
        }
        finally
        {
            InFlight--;
        }
    }

    public bool CanExportOnServer(ReportDescription description) => ServerExport;

    public async Task ExportCsvAsync(ReportDescription description, string filtersJson, Stream destination, CancellationToken cancellationToken)
    {
        ServerExports++;
        await destination.WriteAsync(Encoding.UTF8.GetBytes("server,export\r\n"), cancellationToken).ConfigureAwait(false);
    }
}

public sealed class FakeReportShell : IReportShell
{
    public List<(string Name, string Label, IReadOnlyDictionary<string, string>? Parameters)> Opened { get; } = [];

    public string? Text { get; set; } = "Mine";

    public void OpenReport(string name, string label, IReadOnlyDictionary<string, string>? parameters) => Opened.Add((name, label, parameters));

    public Task<string?> AskTextAsync(string title, string prompt) => Task.FromResult(Text);
}

public sealed class MemorySink : IReportExportSink
{
    public MemoryStream Stream { get; private set; } = new();

    public string? Name { get; private set; }

    public bool Cancel { get; set; }

    public string Text => Encoding.UTF8.GetString(Stream.ToArray());

    public Task<Stream?> OpenAsync(string suggestedFileName, CancellationToken cancellationToken)
    {
        Name = suggestedFileName;
        Stream = new MemoryStream();
        return Task.FromResult<Stream?>(Cancel ? null : Stream);
    }
}

public sealed class CaptureClipboard : Spotledger.Client.Grid.IClipboardSink
{
    public string? LastText { get; private set; }

    public Task SetTextAsync(string text, CancellationToken cancellationToken = default)
    {
        LastText = text;
        return Task.CompletedTask;
    }
}
