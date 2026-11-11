using System.Net;
using System.Text;
using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Models;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class NativeLookupTests
{
    [Fact]
    public async Task UsesTheNativeLookupOperationWithoutSendingASeam()
    {
        var handler = new LookupHttpHandler();
        using var connection = new ApiConnection(handler, new InMemorySecretStore());
        connection.Connect(new Uri("https://site.example"));
        var source = new NativeLookupSource(connection, "document", "reference", () => new LookupRequest { Doc = new LookupRequest_doc() },
            (id, token) => Task.FromResult<LookupItem?>(new(id, "Saved")));
        var rows = await source.SearchAsync("typed", CancellationToken.None);
        Assert.Equal("/api/v1/lookup/document/reference", handler.Path);
        Assert.Contains("\"text\":\"typed\"", handler.Body);
        Assert.DoesNotContain("seam", handler.Body);
        Assert.Equal("Description", Assert.Single(rows).Description);
        Assert.Equal("Saved", (await source.ResolveAsync("id", CancellationToken.None))!.Label);
        var result = await source.SearchResultAsync("typed", CancellationToken.None);
        Assert.Equal("Code", Assert.Single(result.Columns).Label);
        Assert.Equal("code-cell", Assert.Single(result.Rows).Cells["code"]);
        Assert.Equal("Label", Assert.Single(result.Rows).Label);
    }
    private sealed class LookupHttpHandler : HttpMessageHandler
    {
        public string? Body { get; private set; }
        public string? Path { get; private set; }
        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            Path = request.RequestUri!.AbsolutePath;
            Body = await request.Content!.ReadAsStringAsync(cancellationToken);
            return new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent("""{"data":{"columns":[{"name":"code","label":"Code"}],"rows":[{"id":"id","label":"Label","description":"Description","cells":{"code":"code-cell"}}],"has_more":false}}""", Encoding.UTF8, "application/json") };
        }
    }
}
