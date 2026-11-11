using System.Net;
using System.Net.Http.Headers;
using Xunit;

namespace Spotledger.Client.Analytics.Tests;

/// <summary>
/// The sink's four refusals: a non-https URI, a redirect, a client that would tie a batch to the
/// user, and a missing ingest token. Each test also asserts that no request left the process when
/// the sink refused.
/// </summary>
public sealed class HttpAnalyticsSinkTests
{
    private const string Token = "deployment-ingest-token-0001";

    private static readonly DateOnly Day = new(2026, 10, 23);

    private static AnalyticsPayload Payload() =>
        new(AnalyticsPayload.CurrentSchemaVersion, InstallId.Create(), Day,
            [new AnalyticsCounter(AnalyticsEvent.ScreenOpened, "a.b", null, 1)], 0, 0);

    [Fact]
    public async Task An_http_endpoint_is_refused_and_no_request_is_made()
    {
        var handler = new RecordingHandler();
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("http://example.test/"), transport: handler);

        await Assert.ThrowsAsync<AnalyticsEndpointNotSecureException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.Equal(0, handler.Calls);
    }

    [Fact]
    public async Task An_https_endpoint_is_accepted_and_carries_only_the_ingest_token()
    {
        var handler = new RecordingHandler();
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("https://example.test/"), transport: handler);

        await sink.SendAsync(Payload(), CancellationToken.None);

        Assert.Equal(1, handler.Calls);
        Assert.Equal("https://example.test/api/telemetry/usage", handler.LastRequest!.RequestUri!.ToString());
        Assert.Equal(Token, handler.IngestToken);
        Assert.Null(handler.Authorization);
        Assert.DoesNotContain(Token, handler.Body ?? string.Empty, StringComparison.Ordinal); // never in the body
        Assert.Contains("\"screen_opened\"", handler.Body ?? string.Empty, StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("http://localhost:8080/")]
    [InlineData("http://127.0.0.1:8080/")]
    [InlineData("http://[::1]:8080/")]
    public async Task A_loopback_http_endpoint_needs_the_constructor_flag(string baseAddress)
    {
        var refused = new RecordingHandler();
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri(baseAddress), transport: refused);
        await Assert.ThrowsAsync<AnalyticsEndpointNotSecureException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.Equal(0, refused.Calls);

        var accepted = new RecordingHandler();
        using var optIn = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri(baseAddress), allowLoopbackHttp: true, transport: accepted);
        await optIn.SendAsync(Payload(), CancellationToken.None);
        Assert.Equal(1, accepted.Calls);
    }

    [Fact]
    public async Task The_loopback_flag_does_not_excuse_a_remote_host()
    {
        var handler = new RecordingHandler();
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("http://dev.internal.example/"), allowLoopbackHttp: true, transport: handler);

        await Assert.ThrowsAsync<AnalyticsEndpointNotSecureException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.Equal(0, handler.Calls);
    }

    [Fact]
    public async Task A_redirect_is_a_failure_and_is_not_followed()
    {
        var handler = new RecordingHandler
        {
            Status = HttpStatusCode.Found,
            Location = new Uri("http://example.test/api/telemetry/usage"),
        };
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("https://example.test/"), transport: handler);

        await Assert.ThrowsAsync<AnalyticsRedirectedException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.Equal(1, handler.Calls);
    }

    [Fact]
    public void A_client_with_an_authorization_default_header_is_rejected_at_construction()
    {
        using var http = new HttpClient(new RecordingHandler()) { BaseAddress = new Uri("https://example.test/") };
        http.DefaultRequestHeaders.Authorization = new AuthenticationHeaderValue("Bearer", "session-token-of-the-user");

        Assert.Throws<AnalyticsClientNotAnonymousException>(() => new HttpAnalyticsSink(http, () => new FixedAnalyticsTokenSource(Token)));
    }

    [Fact]
    public void A_client_with_any_default_header_is_rejected_at_construction()
    {
        using var http = new HttpClient(new RecordingHandler()) { BaseAddress = new Uri("https://example.test/") };
        http.DefaultRequestHeaders.Add("X-Api-Key", "user-api-key");

        Assert.Throws<AnalyticsClientNotAnonymousException>(() => new HttpAnalyticsSink(http, () => new FixedAnalyticsTokenSource(Token)));
    }

    [Fact]
    public void A_client_with_a_cookie_container_is_rejected_at_construction()
    {
        using var http = new HttpClient(new HttpClientHandler { CookieContainer = new CookieContainer(), UseCookies = true })
        {
            BaseAddress = new Uri("https://example.test/"),
        };

        Assert.Throws<AnalyticsClientNotAnonymousException>(() => new HttpAnalyticsSink(http, () => new FixedAnalyticsTokenSource(Token)));
    }

    [Fact]
    public void A_transport_that_uses_cookies_or_follows_redirects_is_rejected_at_construction()
    {
        Assert.Throws<AnalyticsClientNotAnonymousException>(() =>
            new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("https://example.test/"), transport: new SocketsHttpHandler { UseCookies = true }));

        Assert.Throws<AnalyticsClientNotAnonymousException>(() =>
            new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("https://example.test/"), transport: new HttpClientHandler { AllowAutoRedirect = true }));
    }

    [Fact]
    public void A_transport_chain_with_an_authentication_handler_is_rejected_at_construction()
    {
        using var inner = new RecordingHandler();
        using var auth = new SessionBearerHandler { InnerHandler = inner };

        Assert.Throws<AnalyticsClientNotAnonymousException>(() =>
            new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Token), new Uri("https://example.test/"), transport: auth));
    }

    [Fact]
    public async Task No_ingest_token_means_nothing_is_sent()
    {
        var handler = new RecordingHandler();
        using var sink = new HttpAnalyticsSink(() => FixedAnalyticsTokenSource.None, new Uri("https://example.test/"), transport: handler);

        await Assert.ThrowsAsync<AnalyticsNoIngestTokenException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.Equal(0, handler.Calls);
    }

    [Fact]
    public async Task A_malformed_ingest_token_is_refused_and_never_echoed()
    {
        var handler = new RecordingHandler();
        const string Bad = "not a token: it has spaces";
        using var sink = new HttpAnalyticsSink(() => new FixedAnalyticsTokenSource(Bad), new Uri("https://example.test/"), transport: handler);

        var error = await Assert.ThrowsAsync<AnalyticsInvalidIngestTokenException>(async () => await sink.SendAsync(Payload(), CancellationToken.None));
        Assert.DoesNotContain("spaces", error.Message, StringComparison.Ordinal);
        Assert.Equal(0, handler.Calls);
    }

    [Theory]
    [InlineData("http://localhost:8080/", true)]
    [InlineData("http://127.0.0.1/", true)]
    [InlineData("http://[::1]/", true)]
    [InlineData("http://localhost.example/", false)]
    [InlineData("https://example.test/", false)]
    public void Loopback_detection_is_by_literal_host(string value, bool expected)
    {
        Assert.Equal(expected, HttpAnalyticsSink.IsLoopbackHost(new Uri(value)));
    }

    [Theory]
    [InlineData("deployment-ingest-token-0001", true)]
    [InlineData("short", false)]
    [InlineData("has spaces and spaces yes", false)]
    [InlineData("line\nbreak-token-injected-1", false)]
    public void Token_shape_is_checked(string token, bool expected)
    {
        Assert.Equal(expected, HttpAnalyticsSink.IsWellFormedToken(token));
    }

    private sealed class SessionBearerHandler : DelegatingHandler
    {
    }

    private sealed class RecordingHandler : HttpMessageHandler
    {
        public HttpStatusCode Status { get; set; } = HttpStatusCode.NoContent;

        public Uri? Location { get; set; }

        public int Calls { get; private set; }

        public HttpRequestMessage? LastRequest { get; private set; }

        public string? Body { get; private set; }

        public string? IngestToken { get; private set; }

        public string? Authorization { get; private set; }

        protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            Calls++;
            LastRequest = request;
            IngestToken = request.Headers.TryGetValues(HttpAnalyticsSink.IngestTokenHeader, out var values) ? string.Join(",", values) : null;
            Authorization = request.Headers.Authorization?.ToString();
            Body = request.Content is null ? null : await request.Content.ReadAsStringAsync(cancellationToken);

            var response = new HttpResponseMessage(Status);
            if (Location is not null)
            {
                response.Headers.Location = Location;
            }

            return response;
        }
    }
}
