using System.Net;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Api;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed class NativeGatewayTests
{
    private sealed class Recorder : HttpMessageHandler
    {
        public List<(string Method, string Path, string? Body)> Requests { get; } = [];

        public Func<HttpRequestMessage, (HttpStatusCode, string)> Respond { get; set; } = _ => (HttpStatusCode.OK, "{}");

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellationToken);
            Requests.Add((request.Method.Method, request.RequestUri!.PathAndQuery, body));
            var (status, text) = Respond(request);
            return new HttpResponseMessage(status) { Content = new StringContent(text, Encoding.UTF8, "application/json") };
        }
    }

    private static (NativeReportGateway Gateway, Recorder Handler, ApiConnection Connection) Make()
    {
        var handler = new Recorder();
        var connection = new ApiConnection(handler, new InMemorySecretStore());
        connection.Connect(new Uri("https://site.example"));
        return (new NativeReportGateway(connection), handler, connection);
    }

    [Fact]
    public async Task TheCatalogueIsReadFromTheListOperation()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, """{"data":[{"name":"aging","label":"Receivables aging","app":"accounts","base_doctype":"sales_invoice"},{"name":"tb","label":"Trial balance","app":"accounts","base_doctype":"gl_entry"}]}""");

        var reports = await gateway.ListAsync(CancellationToken.None);

        Assert.Equal(("GET", "/api/v1/report"), (handler.Requests[0].Method, handler.Requests[0].Path));
        Assert.Equal(["aging", "tb"], reports.Select(r => r.Name));
        Assert.Equal("Receivables aging", reports[0].Label);
    }

    [Fact]
    public async Task ADescriptionKeepsItsParametersColumnsAndRowLimit()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, Fx.AllTypesDescription);

        var description = await gateway.DescribeAsync("sales_by_customer", CancellationToken.None);

        Assert.Equal("/api/v1/report/sales_by_customer", handler.Requests[0].Path);
        Assert.Equal(7, description.Parameters.Count);
        Assert.Equal(ReportParameterType.Link, description.Parameters[0].Type);
        Assert.Equal("customer", description.Parameters[0].Options!.LinkDoctype);
        Assert.Equal(["Open", "Paid", "Overdue"], description.Parameters[5].Options!.Values);
        Assert.Equal(4, description.Columns.Count);
        Assert.Equal(50000, description.MaxRows);
    }

    [Fact]
    public async Task ARunPostsTheFiltersAndTheLimitAndReturnsTheRowsWithEveryDigit()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, """
            {"data":{"columns":[{"name":"account","label":"Account","type":"link"},{"name":"balance","label":"Balance","type":"decimal"}],
              "rows":[["0192f3c4-0000-7000-8000-00000000a001","12345678901234567890123456789.01"],["x",null]],"truncated":true},
             "messages":[{"level":"info","code":"report.columns_removed","message":"1 column was removed."}]}
            """);

        var result = await gateway.RunAsync("aging", """{"as_of":"2026-09-30","min_lines":3,"note":"a \"quoted\" thing"}""", 500, CancellationToken.None);

        var request = handler.Requests[0];
        Assert.Equal(("POST", "/api/v1/report/aging/run"), (request.Method, request.Path));
        using var body = JsonDocument.Parse(request.Body!);
        Assert.Equal(500, body.RootElement.GetProperty("limit").GetInt32());
        var filters = body.RootElement.GetProperty("filters");
        Assert.Equal("2026-09-30", filters.GetProperty("as_of").GetString());
        Assert.Equal(3, filters.GetProperty("min_lines").GetInt32());
        Assert.Equal("a \"quoted\" thing", filters.GetProperty("note").GetString());
        Assert.True(result.Truncated);
        Assert.Equal("12345678901234567890123456789.01", result.Rows[0][1]);
        Assert.Null(result.Rows[1][1]);
        Assert.Equal(["account", "balance"], result.Columns.Select(c => c.Name));
        Assert.Equal(ReportJson.ColumnsRemovedCode, Assert.Single(result.Messages).Code);
    }

    [Fact]
    public async Task ABadArgumentArrivesAsATypedErrorWithItsFieldList()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.UnprocessableEntity, """{"error":{"code":"report.bad_argument","message":"The argument min_lines does not fit.","retryable":false,"fields":[{"field":"min_lines","code":"report.bad_argument","message":"Whole number expected."}]}}""");

        var failure = await Assert.ThrowsAsync<ApiRequestException>(() => gateway.RunAsync("aging", "{}", 10, CancellationToken.None));

        Assert.Equal("report.bad_argument", failure.Error.Code);
        Assert.Equal(422, failure.Error.StatusCode);
        Assert.Equal("min_lines", Assert.Single(failure.Error.FieldErrors).Field);
    }

    [Fact]
    public async Task ANotFoundReportIsTheContractsNotFoundError()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.NotFound, """{"error":{"code":"report.not_found","message":"No report with this name.","retryable":false}}""");

        var failure = await Assert.ThrowsAsync<ApiRequestException>(() => gateway.DescribeAsync("nope", CancellationToken.None));

        Assert.Equal("report.not_found", failure.Error.Code);
    }

    [Fact]
    public async Task TheContractHasNoReportExportSoTheGatewayAdmitsIt()
    {
        var (gateway, _, connection) = Make();
        using var _ = connection;

        Assert.False(gateway.CanExportOnServer(Fx.Simple()));
        await Assert.ThrowsAsync<NotSupportedException>(() => gateway.ExportCsvAsync(Fx.Simple(), "{}", new MemoryStream(), CancellationToken.None));
    }
}
