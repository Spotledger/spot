using System.Net;
using System.Net.Http.Headers;
using System.Reflection;
using System.Text;

namespace Spotledger.Client.Analytics;

/// <summary>
/// Posts a batch to the server's telemetry endpoint, and refuses to do so unless the batch cannot
/// be tied to the user and cannot travel in the clear:
/// <list type="bullet">
/// <item>the effective request URI (base address plus <see cref="DefaultPath"/>) must be
/// <c>https</c>; the only exception is a loopback host, and only when the host passed
/// <c>allowLoopbackHttp: true</c> because its own server URL is loopback (never from a user
/// setting a user edits). A refusal throws <see cref="AnalyticsEndpointNotSecureException"/> and no
/// request is made;</item>
/// <item>redirects are not followed and a 3xx is a failure
/// (<see cref="AnalyticsRedirectedException"/>), so an <c>https</c> endpoint cannot bounce a batch
/// to <c>http</c>;</item>
/// <item>the client is anonymous: no default header, no cookie container, no redirect-following or
/// authentication handler, and the only credential sent is the ingest token from
/// <see cref="IAnalyticsTokenSource"/> - never the user's session token. A client that would break
/// that is refused at construction (<see cref="AnalyticsClientNotAnonymousException"/>);</item>
/// <item>with no ingest token configured nothing is sent
/// (<see cref="AnalyticsNoIngestTokenException"/>): the counters stay local.</item>
/// </list>
/// The token is never logged and never appears in an exception message.
/// </summary>
public sealed class HttpAnalyticsSink : IAnalyticsSink, IDisposable
{
    /// <summary>Relative path of the endpoint (owner: the telemetry endpoint, see analytics-events.md).</summary>
    public const string DefaultPath = "api/telemetry/usage";

    /// <summary>The only credential the sink sends. Deliberately not <c>Authorization</c>: it is not a user session.</summary>
    public const string IngestTokenHeader = "X-Spot-Ingest-Token";

    /// <summary>Shortest ingest token accepted (a deployment setting issued by the endpoint owner).</summary>
    public const int MinTokenLength = 16;

    public const int MaxTokenLength = 512;

    private readonly HttpClient _http;
    private readonly Func<IAnalyticsTokenSource> _ingestTokens;
    private readonly string _path;
    private readonly bool _allowLoopbackHttp;
    private readonly bool _ownsClient;
    private bool _disposed;

    /// <summary>
    /// Builds its own client: no default headers, no cookie container, no automatic redirects.
    /// <paramref name="transport"/> is for tests and for a host that supplies its own anonymous
    /// transport; it is validated the same way production is.
    /// </summary>
    public HttpAnalyticsSink(
        Func<IAnalyticsTokenSource> ingestTokens,
        Uri baseAddress,
        bool allowLoopbackHttp = false,
        string path = DefaultPath,
        HttpMessageHandler? transport = null)
    {
        ArgumentNullException.ThrowIfNull(ingestTokens);
        ArgumentNullException.ThrowIfNull(baseAddress);
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        if (!baseAddress.IsAbsoluteUri)
        {
            throw new ArgumentException("The telemetry endpoint base address must be absolute.", nameof(baseAddress));
        }

        if (transport is not null)
        {
            ValidateTransport(transport, nameof(transport));
        }

        var handler = transport ?? new SocketsHttpHandler
        {
            AllowAutoRedirect = false,
            UseCookies = false,
            UseProxy = false,
        };
        _http = new HttpClient(handler, disposeHandler: true)
        {
            BaseAddress = baseAddress,
            Timeout = TimeSpan.FromSeconds(30),
        };
        _ownsClient = true;
        _ingestTokens = ingestTokens;
        _path = path;
        _allowLoopbackHttp = allowLoopbackHttp;
    }

    /// <summary>
    /// Uses an explicitly constructed client, which must be anonymous: no default request header at
    /// all (so no <c>Authorization</c>), no cookie container, and no redirect-following or
    /// authentication-looking handler in its chain. The sink still refuses a non-https URI.
    /// </summary>
    public HttpAnalyticsSink(HttpClient http, Func<IAnalyticsTokenSource> ingestTokens, bool allowLoopbackHttp = false, string path = DefaultPath)
    {
        ArgumentNullException.ThrowIfNull(http);
        ArgumentNullException.ThrowIfNull(ingestTokens);
        ArgumentException.ThrowIfNullOrWhiteSpace(path);
        ValidateAnonymousClient(http);
        _http = http;
        _ownsClient = false;
        _ingestTokens = ingestTokens;
        _path = path;
        _allowLoopbackHttp = allowLoopbackHttp;
    }

    /// <summary>The URI this sink would post to (base address plus path); diagnostics only.</summary>
    public Uri RequestUri => new(_http.BaseAddress ?? throw new InvalidOperationException("The client has no base address."), _path);

    public async ValueTask SendAsync(AnalyticsPayload payload, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(payload);
        ObjectDisposedException.ThrowIf(_disposed, this);

        // 1. Transport, before a request object exists: a batch must never leave the machine in the clear.
        var uri = RequestUri;
        EnsureSecure(uri);

        // 2. Credential: only the ingest token travels. This client has no default header and the
        //    sink never touches a token store, so the user's session token cannot be attached.
        var token = ResolveIngestToken();

        using var request = new HttpRequestMessage(HttpMethod.Post, uri);
        request.Headers.TryAddWithoutValidation(IngestTokenHeader, token);
        using var content = new StringContent(
            AnalyticsJson.Serialize(payload),
            Encoding.UTF8,
            new MediaTypeHeaderValue("application/json"));
        request.Content = content;

        using var response = await _http
            .SendAsync(request, HttpCompletionOption.ResponseHeadersRead, cancellationToken)
            .ConfigureAwait(false);

        if ((int)response.StatusCode is >= 300 and < 400)
        {
            throw new AnalyticsRedirectedException();
        }

        // Any other non-2xx throws; UsageAnalytics keeps the counters and the batch is retried.
        response.EnsureSuccessStatusCode();
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        if (_ownsClient)
        {
            _http.Dispose();
        }
    }

    /// <summary>True for the hosts the loopback exception covers (literals only, never a DNS lookup).</summary>
    public static bool IsLoopbackHost(Uri uri)
    {
        ArgumentNullException.ThrowIfNull(uri);
        if (uri.IsLoopback)
        {
            return true;
        }

        return uri.Host.ToLowerInvariant() switch
        {
            "localhost" or "127.0.0.1" or "::1" or "[::1]" => true,
            _ => false,
        };
    }

    /// <summary>A deployment token: printable ASCII without whitespace, so it cannot split a header.</summary>
    public static bool IsWellFormedToken(string? token)
    {
        if (token is null || token.Length is < MinTokenLength or > MaxTokenLength)
        {
            return false;
        }

        foreach (var c in token)
        {
            if (c is not (>= 'a' and <= 'z' or >= 'A' and <= 'Z' or >= '0' and <= '9' or '.' or '_' or '~' or '+' or '/' or '=' or '-'))
            {
                return false;
            }
        }

        return true;
    }

    private void EnsureSecure(Uri uri)
    {
        if (uri.Scheme == Uri.UriSchemeHttps)
        {
            if (uri.UserInfo.Length != 0)
            {
                // A credential in the URL ends up in a proxy log; refuse it.
                throw new AnalyticsEndpointNotSecureException("The analytics endpoint URI must not carry user info.");
            }

            return;
        }

        if (uri.Scheme == Uri.UriSchemeHttp && _allowLoopbackHttp && IsLoopbackHost(uri) && uri.UserInfo.Length == 0)
        {
            return;
        }

        // No scheme, host or path in the message: a server URL can be identifying.
        throw new AnalyticsEndpointNotSecureException();
    }

    private string ResolveIngestToken()
    {
        var source = _ingestTokens() ?? throw new AnalyticsNoIngestTokenException();
        var token = source.GetIngestToken();
        if (string.IsNullOrEmpty(token))
        {
            throw new AnalyticsNoIngestTokenException();
        }

        if (!IsWellFormedToken(token))
        {
            throw new AnalyticsInvalidIngestTokenException();
        }

        return token;
    }

    private static void ValidateAnonymousClient(HttpClient http)
    {
        if (http.BaseAddress is null || !http.BaseAddress.IsAbsoluteUri)
        {
            throw new AnalyticsClientNotAnonymousException("The analytics client must have an absolute base address.", nameof(http));
        }

        if (http.DefaultRequestHeaders.Authorization is not null)
        {
            throw new AnalyticsClientNotAnonymousException("The analytics client carries a default Authorization header.", nameof(http));
        }

        if (http.DefaultRequestHeaders.Any())
        {
            throw new AnalyticsClientNotAnonymousException("The analytics client must not carry default request headers.", nameof(http));
        }

        // HttpClient exposes no handler getter, so this probe is best effort: when the handler is
        // visible we reject a cookie container or a redirect-following handler; the header check
        // above still holds when it is not.
        var handler = ProbeHandler(http);
        if (handler is not null)
        {
            ValidateTransport(handler, nameof(http));
        }
    }

    private static HttpMessageHandler? ProbeHandler(HttpClient http)
    {
        try
        {
            var field = typeof(HttpMessageInvoker).GetField("_handler", BindingFlags.Instance | BindingFlags.NonPublic)
                ?? typeof(HttpClient).GetField("_handler", BindingFlags.Instance | BindingFlags.NonPublic);
            return field?.GetValue(http) as HttpMessageHandler;
        }
        catch (Exception)
        {
            return null;
        }
    }

    private static void ValidateTransport(HttpMessageHandler transport, string paramName)
    {
        ArgumentNullException.ThrowIfNull(transport);
        foreach (var handler in Chain(transport))
        {
            switch (handler)
            {
                case HttpClientHandler h when h.UseCookies || h.CookieContainer is not null:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler uses cookies.", paramName);
                case HttpClientHandler h when h.AllowAutoRedirect:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler follows redirects.", paramName);
                case HttpClientHandler h when h.Credentials is not null || h.UseDefaultCredentials:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler carries credentials.", paramName);
                case SocketsHttpHandler s when s.UseCookies:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler uses cookies.", paramName);
                case SocketsHttpHandler s when s.AllowAutoRedirect:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler follows redirects.", paramName);
                case SocketsHttpHandler s when s.Credentials is not null:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler carries credentials.", paramName);
                case SocketsHttpHandler s when s.Proxy?.Credentials is not null:
                    throw new AnalyticsClientNotAnonymousException("The analytics client's proxy handler carries credentials.", paramName);
                case DelegatingHandler d when LooksLikeAuthentication(d):
                    throw new AnalyticsClientNotAnonymousException("The analytics client's handler chain contains an authentication handler.", paramName);
            }
        }
    }

    private static IEnumerable<HttpMessageHandler> Chain(HttpMessageHandler handler)
    {
        var current = handler;
        var seen = new HashSet<HttpMessageHandler>();
        while (current is not null && seen.Add(current))
        {
            yield return current;
            current = current is DelegatingHandler d ? d.InnerHandler : null;
        }
    }

    private static bool LooksLikeAuthentication(HttpMessageHandler handler)
    {
        var name = handler.GetType().FullName ?? handler.GetType().Name;
        return name.Contains("Auth", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Bearer", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Session", StringComparison.OrdinalIgnoreCase)
            || name.Contains("Credential", StringComparison.OrdinalIgnoreCase);
    }
}
