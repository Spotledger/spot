using System.Net;
using System.Text;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The in-process fake server: answers the native-API contract endpoints the shell uses
/// with small JSON bodies shaped exactly like the contract's examples and schemas.
/// Records every request (method, path, headers) so tests can assert the bearer header.
/// </summary>
public sealed class FakeServerHandler : HttpMessageHandler
{
    public sealed record RecordedRequest(string Method, string Path, string? Authorization, string? Body);

    private readonly object _requestsLock = new();
    private readonly List<RecordedRequest> _requests = new();

    /// <summary>A snapshot of the requests so far; recording is locked because concurrent calls reach the handler on pool threads.</summary>
    public IReadOnlyList<RecordedRequest> Requests
    {
        get
        {
            lock (_requestsLock)
            {
                return _requests.ToArray();
            }
        }
    }

    private int _phaseArrivals;

    /// <summary>The recorded requests for one endpoint path, in order.</summary>
    public IEnumerable<RecordedRequest> RequestsFor(string path) =>
        Requests.Where(request => string.Equals(request.Path, path, StringComparison.Ordinal));

    /// <summary>Scripted answers per endpoint path; tests mutate these.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage> ReadyResponder { get; set; } = _ =>
        Json(HttpStatusCode.OK, """{"status":"ready","checks":[{"name":"primary_connection","ok":true,"detail":"round trip 1.2 ms"}]}""");

    public Func<HttpRequestMessage, HttpResponseMessage> LoginResponder { get; set; } = _ =>
        Json(HttpStatusCode.OK, """
        {"data":{"token":"3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs","expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"}}}
        """);

    public Func<HttpRequestMessage, HttpResponseMessage> SessionResponder { get; set; } = _ =>
        Json(HttpStatusCode.OK, """
        {"data":{"expires_at":"2026-09-30T08:00:00Z","user":{"id":"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f","email":"ayesha@acme.test","full_name":"Ayesha Khan"},"site":"acme.test"}}
        """);

    public Func<HttpRequestMessage, HttpResponseMessage> LogoutResponder { get; set; } = _ =>
        new HttpResponseMessage(HttpStatusCode.NoContent);

    /// <summary>The body GET /api/health/live answers with; tests set version strings here.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage> LiveResponder { get; set; } = _ =>
        Json(HttpStatusCode.OK, """{"status":"live"}""");

    /// <summary>POST /api/v1/auth/password (contract 0.3.0): 204 by default.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage> PasswordResponder { get; set; } = _ =>
        new HttpResponseMessage(HttpStatusCode.NoContent);

    /// <summary>GET /api/v1/auth/sessions (contract 0.3.0): a two-row list by default.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage> SessionsListResponder { get; set; } = _ =>
        Json(HttpStatusCode.OK, """
        {"data":[{"id":"sess-1","created_at":"2026-09-29T08:00:00Z","last_used_at":"2026-09-30T07:55:00Z","user_agent":"Spotledger desktop","current":true},{"id":"sess-2","created_at":"2026-09-28T10:00:00Z","last_used_at":"2026-09-30T07:40:00Z","user_agent":"Firefox on Ubuntu","current":false}]}
        """);

    /// <summary>DELETE /api/v1/auth/sessions/{id}: 204 by default.</summary>
    public Func<HttpRequestMessage, HttpResponseMessage> SessionsDeleteResponder { get; set; } = _ =>
        new HttpResponseMessage(HttpStatusCode.NoContent);

    /// <summary>When set, every authorized endpoint answers with this 401 envelope.</summary>
    public string? SessionExpiredCode { get; set; }

    /// <summary>Optional asynchronous gate for deterministic in-flight request tests.</summary>
    public Func<HttpRequestMessage, CancellationToken, Task>? BeforeResponseAsync { get; set; }

    /// <summary>
    /// Deterministic two-phase gate for the shared-prompt tests: phase 1 (<see
    /// cref="CountTo"/> requests, keyed by method+path) the handler waits on
    /// <c>Phase1Gate</c>; when that task completes, phase 2 re-arms for the next batch.
    /// </summary>
    public int CountTo { get; set; }

    public TaskCompletionSource PhaseGate { get; set; } = new(TaskCreationOptions.RunContinuationsAsynchronously);

    protected override async Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        // Force a real asynchronous boundary, like a network response.
        await Task.Delay(1, cancellationToken).ConfigureAwait(false);

        var key = request.Method.Method + " " + request.RequestUri!.AbsolutePath;
        if (CountTo > 0)
        {
            var arrival = Interlocked.Increment(ref _phaseArrivals);
            if (arrival <= CountTo)
            {
                await PhaseGate.Task.ConfigureAwait(false);
                if (arrival == CountTo)
                {
                    // Next batch arrives ungated until the test re-arms the phase.
                    _ = Interlocked.Exchange(ref _phaseArrivals, int.MaxValue);
                }
            }
        }

        if (BeforeResponseAsync is { } beforeResponse)
        {
            await beforeResponse(request, cancellationToken).ConfigureAwait(false);
        }

        var path = request.RequestUri!.AbsolutePath;
        string? authorization = null;
        if (request.Headers.TryGetValues("Authorization", out var values))
        {
            authorization = values.FirstOrDefault();
        }

        // Read the body before dispatch so responders and tests can inspect it; content
        // types used here are buffered and remain re-readable.
        string? body = null;
        if (request.Content is not null)
        {
            body = await request.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
        }

        lock (_requestsLock)
        {
            _requests.Add(new RecordedRequest(request.Method.Method, path, authorization, body));
        }

        if (SessionExpiredCode is not null && path.StartsWith("/api/v1/auth/session", StringComparison.Ordinal))
        {
            return Error(HttpStatusCode.Unauthorized, SessionExpiredCode, "The session has ended; sign in again.");
        }

        return path switch
        {
            "/api/health/ready" => ReadyResponder(request),
            "/api/health/live" => LiveResponder(request),
            "/api/v1/auth/login" => LoginResponder(request),
            "/api/v1/auth/session" => SessionResponder(request),
            "/api/v1/auth/logout" => LogoutResponder(request),
            "/api/v1/auth/password" => PasswordResponder(request),
            "/api/v1/auth/sessions" => SessionsListResponder(request),
            _ when path.StartsWith("/api/v1/auth/sessions/", StringComparison.Ordinal) => SessionsDeleteResponder(request),
            _ => Error(HttpStatusCode.NotFound, "http.unknown_path", "The path is not part of the contract."),
        };
    }

    /// <summary>A contract ErrorEnvelope body with the given code and message.</summary>
    public static HttpResponseMessage Error(HttpStatusCode status, string code, string message, string? fieldsJson = null)
    {
        var fields = fieldsJson is null ? string.Empty : ",\"fields\":" + fieldsJson;
        var body = "{\"error\":{\"code\":\"" + code + "\",\"message\":\"" + message + "\""
            + fields + ",\"retryable\":false,\"request_id\":\"req-7f3a\"}}";
        return Json(status, body);
    }

    public static HttpResponseMessage Json(HttpStatusCode status, string body) =>
        new(status)
        {
            Content = new StringContent(body, Encoding.UTF8, "application/json"),
        };
}
