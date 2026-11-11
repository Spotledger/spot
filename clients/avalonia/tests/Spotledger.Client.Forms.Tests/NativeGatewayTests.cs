using System.Net;
using System.Text;
using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

public sealed class NativeGatewayTests
{
    private sealed class Recorder : HttpMessageHandler
    {
        public List<(string Method, string Path, string? Body, IDictionary<string, string> Headers)> Requests { get; } = [];
        public Func<HttpRequestMessage, (HttpStatusCode, string)> Respond { get; set; } = _ => (HttpStatusCode.OK, "{}");

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            var body = request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellationToken);
            Requests.Add((request.Method.Method, request.RequestUri!.PathAndQuery, body,
                request.Headers.ToDictionary(h => h.Key, h => string.Join(",", h.Value), StringComparer.OrdinalIgnoreCase)));
            var (status, text) = Respond(request);
            return new HttpResponseMessage(status) { Content = new StringContent(text, Encoding.UTF8, "application/json") };
        }
    }

    private static (NativeDocumentGateway Gateway, Recorder Handler, ApiConnection Connection) Make()
    {
        var handler = new Recorder();
        var connection = new ApiConnection(handler, new InMemorySecretStore());
        connection.Connect(new Uri("https://site.example"));
        return (new NativeDocumentGateway(connection, new MetaCache(connection)), handler, connection);
    }

    private const string Document = """{"data":{"id":"INV-1","version":3,"_state":"draft","title":"T","items":[{"id":"r1","qty":"2.00"}]},"messages":[{"level":"warning","code":"w.1","message":"Heads up"}]}""";

    [Fact]
    public async Task LoadReadsTheDocumentAndItsMessages()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, Document);

        var reply = await gateway.LoadAsync("invoice", "INV-1", CancellationToken.None);

        var request = Assert.Single(handler.Requests);
        Assert.Equal(("GET", "/api/v1/resource/invoice/INV-1"), (request.Method, request.Path));
        using var parsed = System.Text.Json.JsonDocument.Parse(reply.DataJson);
        Assert.Equal("T", parsed.RootElement.GetProperty("title").GetString());
        Assert.Equal(3, parsed.RootElement.GetProperty("version").GetInt32());
        var message = Assert.Single(reply.Messages);
        Assert.Equal(("warning", "w.1", "Heads up"), (message.Level, message.Code, message.Text));
    }

    [Fact]
    public async Task PatchSendsTheMergePatchWithIfMatchAtTheLoadedVersion()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, Document);

        await gateway.PatchAsync("invoice", "INV-1", """{"title":"New"}""", 3, CancellationToken.None);

        var request = Assert.Single(handler.Requests);
        Assert.Equal("PATCH", request.Method);
        Assert.Equal("\"3\"", request.Headers["If-Match"]);
        Assert.Contains("\"title\":\"New\"", request.Body, StringComparison.Ordinal);
    }

    [Fact]
    public async Task CreateAndLifecycleCarryAnIdempotencyKeyAndLifecycleCarriesIfMatch()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK, Document);

        await gateway.CreateAsync("invoice", """{"title":"New"}""", CancellationToken.None);
        await gateway.LifecycleAsync("invoice", "INV-1", LifecycleAction.Submit, 3, null, CancellationToken.None);
        await gateway.LifecycleAsync("invoice", "INV-1", LifecycleAction.Cancel, 4, "why", CancellationToken.None);
        await gateway.LifecycleAsync("invoice", "INV-1", LifecycleAction.Correct, 5, null, CancellationToken.None);

        Assert.Equal(
            [("POST", "/api/v1/resource/invoice"), ("POST", "/api/v1/resource/invoice/INV-1/post"),
                ("POST", "/api/v1/resource/invoice/INV-1/cancel"), ("POST", "/api/v1/resource/invoice/INV-1/correct")],
            handler.Requests.Select(r => (r.Method, r.Path)));
        Assert.All(handler.Requests, r => Assert.True(r.Headers.ContainsKey("Idempotency-Key")));
        Assert.Equal(["\"3\"", "\"4\"", "\"5\""], handler.Requests.Skip(1).Select(r => r.Headers["If-Match"]));
    }

    [Fact]
    public async Task DeleteSendsIfMatch()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.NoContent, string.Empty);

        await gateway.DeleteAsync("invoice", "INV-1", 3, CancellationToken.None);

        var request = Assert.Single(handler.Requests);
        Assert.Equal("DELETE", request.Method);
        Assert.Equal("\"3\"", request.Headers["If-Match"]);
    }

    [Fact]
    public async Task AContractErrorBecomesAnApiRequestExceptionWithItsFields()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.UnprocessableContent,
            """{"error":{"code":"document.validation_failed","message":"Not valid","request_id":"r","retryable":false,"fields":[{"field":"title","code":"document.required","message":"Required"}]}}""");

        var failure = await Assert.ThrowsAsync<ApiRequestException>(
            () => gateway.PatchAsync("invoice", "INV-1", "{}", 3, CancellationToken.None));

        Assert.Equal("document.validation_failed", failure.Error.Code);
        Assert.Equal("title", Assert.Single(failure.Error.FieldErrors).Field);
    }

    [Fact]
    public async Task ThePermissionSummaryYieldsTheSnapshotAndTheDocumentRights()
    {
        var (gateway, handler, connection) = Make();
        using var _ = connection;
        handler.Respond = _ => (HttpStatusCode.OK,
            """{"data":{"doctypes":[{"doctype":"invoice","rights":{"read":true,"write":true,"create":true,"delete":false,"submit":true,"cancel":false,"correct":false,"rename":false},"fields":{"hidden":[],"read_only":[],"masked":["secret"],"revealable":[]}}]},"etag":"e9"}""");

        var reply = await gateway.GetPermissionsAsync("invoice", CancellationToken.None);

        Assert.Equal("/api/v1/auth/permissions?doctype=invoice", Assert.Single(handler.Requests).Path);
        Assert.Equal(new DocumentRights(true, true, true, false, true, false, false, false), reply.Rights);
        Assert.Equal(["secret"], reply.Snapshot.DocTypes["invoice"].Masked);
    }

    [Fact]
    public void RightsOfAnUnrightedDocTypeAreNone()
    {
        Assert.Equal(DocumentRights.None, DocumentRights.Decode("""{"data":{"doctypes":[]}}""", "invoice"));
    }
}

public sealed class NativeLookupWiringTests
{
    [Fact]
    public async Task ALinkLookupSendsTheDraftsCurrentSnapshotToTheLookupOperation()
    {
        var handler = new LookupRecorder();
        using var connection = new ApiConnection(handler, new InMemorySecretStore());
        connection.Connect(new Uri("https://site.example"));
        var gateway = new NativeDocumentGateway(connection, new MetaCache(connection));
        var draft = new DocumentDraft(Fixtures.Model());
        draft.TrySetText("title", "Typed title");

        var source = gateway.LookupFor("invoice", "customer", draft.LookupSnapshot)!;
        var rows = await source.SearchAsync("cu", CancellationToken.None);

        Assert.Equal("/api/v1/lookup/invoice/customer", handler.Path);
        Assert.Contains("\"text\":\"cu\"", handler.Body, StringComparison.Ordinal);
        Assert.Contains("\"title\":\"Typed title\"", handler.Body, StringComparison.Ordinal);
        Assert.Equal("CUST-1", Assert.Single(rows).Id);

        draft.TrySetText("title", "Changed since");
        await source.SearchAsync("cu", CancellationToken.None);
        Assert.Contains("\"title\":\"Changed since\"", handler.Body, StringComparison.Ordinal);
    }

    private sealed class LookupRecorder : HttpMessageHandler
    {
        public string? Path { get; private set; }
        public string? Body { get; private set; }

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            Path = request.RequestUri!.AbsolutePath;
            Body = await request.Content!.ReadAsStringAsync(cancellationToken);
            return new HttpResponseMessage(HttpStatusCode.OK)
            {
                Content = new StringContent("""{"data":{"columns":[],"rows":[{"id":"CUST-1","label":"Customer one","cells":{}}],"has_more":false}}""", Encoding.UTF8, "application/json"),
            };
        }
    }
}
