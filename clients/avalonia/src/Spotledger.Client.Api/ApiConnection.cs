using System.Net.Http;
using System.Text.Json;
using Microsoft.Kiota.Abstractions;
using Microsoft.Kiota.Abstractions.Authentication;
using Microsoft.Kiota.Abstractions.Serialization;
using Microsoft.Kiota.Http.HttpClientLibrary;
using Microsoft.Kiota.Serialization.Json;
using Spotledger.Client.Api.Generated;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Api;

/// <summary>
/// The hand-written entry point over the Kiota-generated <see cref="NativeApiClient"/>:
/// owns the base URL, the in-memory session token, the bearer provider wiring, the OS
/// secret store behind "Remember me", the server version negotiation, and the mapping of
/// the contract's error envelope into <see cref="ApiError"/>. The shell talks to the
/// server only through this type.
/// Mid-session 401s (<c>auth.session_expired</c> / <c>auth.invalid_credentials</c>) are
/// handled by <see cref="RunSessionAwareAsync{T}"/>: one <see cref="SessionExpired"/>
/// event per expiry batch, one re-sign-in prompt shared by concurrent calls, and exactly
/// one retry of the failed request after a successful prompt.
/// </summary>
public sealed class ApiConnection : IDisposable
{
    /// <summary>The error codes that mean the held session is no longer valid (contract registry).</summary>
    private static readonly string[] SessionBreakingCodes =
    {
        "auth.session_expired",
        "auth.invalid_credentials",
    };

    /// <summary>The JSON body writer for the hand-rolled contract-0.3 endpoints (snake_case).</summary>
    private static readonly JsonSerializerOptions ContractJson = new(JsonSerializerDefaults.Web)
    {
        PropertyNamingPolicy = JsonNamingPolicy.SnakeCaseLower,
    };

    private readonly HttpClient _httpClient;
    private readonly HttpClientRequestAdapter _adapter;
    /// <summary>Depth of aware-call nesting: while positive, a 401 inside a hand-written
    /// helper must not raise the hard expiry event (the aware wrapper owns the prompt).</summary>
    private int _awareCallDepth;
    /// <summary>The token generation the current aware call's request was sent under (null outside one).</summary>
    private readonly AsyncLocal<long?> _callGeneration = new();
    private Task<bool>? _reSignInTask;
    private readonly SemaphoreSlim _reSignInGate = new(1, 1);
    private int _expiryEventRaised;
    private string? _rememberedKey;
    private bool _disposed;

    /// <summary>
    /// Creates a connection. When <paramref name="handler"/> is supplied (the fake server
    /// in tests) it replaces the network handler; production passes nothing. The OS
    /// secret store and remembered-user hint default to the platform choices; tests
    /// inject their own.
    /// </summary>
    public ApiConnection(
        HttpMessageHandler? handler = null,
        ISecretStore? secretStore = null,
        RememberedUserStore? rememberedUsers = null)
    {
        Tokens = new SessionTokenStore();
        Secrets = secretStore ?? SecretStores.CreateDefault();
        Remembered = rememberedUsers ?? new RememberedUserStore();
        _httpClient = handler is null ? new HttpClient() : new HttpClient(handler, disposeHandler: true);
        var auth = new BaseBearerTokenAuthenticationProvider(Tokens);
        _adapter = new HttpClientRequestAdapter(auth, new JsonParseNodeFactory(), new JsonSerializationWriterFactory(), _httpClient);
        Client = new NativeApiClient(_adapter);
    }

    /// <summary>The base URL of the site this connection currently targets.</summary>
    public Uri? BaseUri { get; private set; }

    /// <summary>The in-memory session token store, also wired as the bearer token provider.</summary>
    public SessionTokenStore Tokens { get; }

    /// <summary>The OS secret store backing "Remember me"; only the token is ever written, never a password.</summary>
    public ISecretStore Secrets { get; }

    /// <summary>The non-secret hint of which user last remembered a token per server.</summary>
    public RememberedUserStore Remembered { get; }

    /// <summary>The generated request-builder root. Use the typed helpers below where they exist.</summary>
    public NativeApiClient Client { get; }

    /// <summary>The resolved capabilities of the connected server; assumptions before negotiation.</summary>
    public ServerCapabilities Capabilities { get; private set; } = ServerCapabilities.AssumedCurrent();

    /// <summary>
    /// The re-sign-in prompt behind "Remember me" expiry: shown by the shell as a modal
    /// over the current screen. True return = the user signed in again (the token is
    /// already set); false = cancelled. Concurrent 401s share one invocation.
    /// </summary>
    public Func<Task<bool>>? ReSignInProvider { get; set; }

    /// <summary>True while the re-sign-in modal is being shown.</summary>
    public bool IsReSignInActive { get; private set; }

    /// <summary>
    /// Raised when an authorized call comes back 401 with <c>auth.session_expired</c> or
    /// <c>auth.invalid_credentials</c>: the session token has been cleared. Concurrent
    /// calls share one event per expiry batch; hard paths (connection setup, direct
    /// helpers) also raise it exactly once per batch.
    /// </summary>
    public event EventHandler? SessionExpired;

    /// <summary>
    /// Test-only seam (InternalsVisibleTo <c>Spotledger.Client.Shell.Tests</c>): awaited
    /// between a caller's own "is this 401 stale?" decision and the re-sign-in gate, which is
    /// the window where another call's prompt can finish and install a newer session. Null in
    /// production, so the product awaits nothing extra (WP-TST-14.T7).
    /// </summary>
    internal Func<Task>? BeforeReSignInGateAsync { get; set; }

    /// <summary>Points the connection at a site (the server picker's chosen URL).</summary>
    public void Connect(Uri baseUri)
    {
        ArgumentNullException.ThrowIfNull(baseUri);
        BaseUri = baseUri;
        _adapter.BaseUrl = baseUri.GetLeftPart(UriPartial.Authority);
    }

    // ---- session resume (WP-UI-02.T3 deliverable 2) ----

    /// <summary>
    /// Start-up resume: when the user last chose "Remember me" on this server, the stored
    /// token is loaded from the OS secret store and proven with <c>GET
    /// /api/v1/auth/session</c>. A valid token lands in the shell without a form; an
    /// expired one is deleted and the caller shows the sign-in step pre-filled with the
    /// server and user. Nothing here sees a password.
    /// </summary>
    public async Task<SessionResumeResult> ResumeSessionAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        var server = BaseUri!.GetLeftPart(UriPartial.Authority);
        var user = Remembered.Load(server);
        if (user is null)
        {
            return SessionResumeResult.None();
        }

        var key = SecretKey.For(server, user);
        var stored = await Secrets.GetSecretAsync(key, cancellationToken).ConfigureAwait(false);
        if (string.IsNullOrEmpty(stored))
        {
            // The hint points at a user whose token is gone from the keyring (another
            // device's sign-out, a cleared store): fall back to a fresh sign-in.
            Remembered.Forget(server);
            return SessionResumeResult.None();
        }

        _rememberedKey = key;
        Tokens.SetToken(stored);
        try
        {
            var session = await LoadSessionAsync(cancellationToken).ConfigureAwait(false);
            return SessionResumeResult.Resumed(user, session);
        }
        catch (ApiRequestException failure) when (failure.Error.StatusCode == 401)
        {
            // Expired or revoked while the app was closed: drop the stale token and the
            // hint so the sign-in step is shown instead. The user name prefills the form.
            await DeleteStoredTokenAsync(key, server, cancellationToken).ConfigureAwait(false);
            return SessionResumeResult.Rejected(user);
        }
        catch (Exception) when (IsResumeFailure())
        {
            // The keyring held a token but the server is unreachable (or answered with a
            // non-contract error): keep the stored token for a later retry and let the
            // caller show its offline message instead of a sign-in form.
            Tokens.ClearToken();
            return SessionResumeResult.Failed("The session could not be checked. Check the connection and try again.");
        }
    }

    /// <summary>Everything the resume check must catch besides its own 401 and cancellation:
    /// any exception type other than those handled explicitly (transport, dispose races).</summary>
    private static bool IsResumeFailure() => true;

    /// <summary>Stores the session token behind "Remember me" after a successful sign-in; the key is <c>&lt;server&gt;/&lt;user&gt;</c>.</summary>
    public async Task RememberTokenAsync(string user, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(user);
        RequireConnected();
        var server = BaseUri!.GetLeftPart(UriPartial.Authority);
        var key = SecretKey.For(server, user);
        var token = Tokens.GetTokenForSecretStore();
        if (string.IsNullOrEmpty(token))
        {
            throw new InvalidOperationException("There is no session token to remember.");
        }

        await Secrets.SetSecretAsync(key, token, cancellationToken).ConfigureAwait(false);
        Remembered.Remember(server, user);
        _rememberedKey = key;
    }

    /// <summary>Deletes the stored token and hint (sign-out of a remembered session). Never touches a password — none is kept.</summary>
    public async Task ForgetStoredTokenAsync(CancellationToken cancellationToken = default)
    {
        if (_rememberedKey is not { } key)
        {
            return;
        }

        var server = key[..key.LastIndexOf('/')];
        await Secrets.DeleteSecretAsync(key, cancellationToken).ConfigureAwait(false);
        Remembered.Forget(server);
        _rememberedKey = null;
    }

    private async Task DeleteStoredTokenAsync(string key, string server, CancellationToken cancellationToken)
    {
        Tokens.ClearToken();
        try
        {
            await Secrets.DeleteSecretAsync(key, cancellationToken).ConfigureAwait(false);
        }
        catch (Exception exception) when (exception is InvalidOperationException or IOException or UnauthorizedAccessException)
        {
            // An unremovable stale token is annoying but not fatal: the in-memory token
            // is gone, so the user signs in and the next Remember overwrites the entry.
        }

        Remembered.Forget(server);
        _rememberedKey = null;
    }

    // ---- version negotiation (WP-UI-02.T3 deliverable 3) ----

    /// <summary>
    /// Reads the server's API version from <c>GET /api/health/live</c> and resolves the
    /// <see cref="Capabilities"/>. A major mismatch refuses with
    /// <see cref="ApiVersionMismatchException"/>; a newer minor resolves a non-blocking
    /// <c>Notice</c>; an older minor disables the features the contract added later.
    /// A server that sends no <c>version</c> (the 0.2.0 contract has no field yet) is
    /// treated as the current contract: <see cref="ServerCapabilities.VersionReported"/>
    /// says which happened.
    /// </summary>
    public async Task<ServerCapabilities> NegotiateVersionAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        string? body;
        try
        {
            var requestInfo = Client.Api.Health.Live.ToGetRequestInformation();
            using var stream = await _adapter.SendPrimitiveAsync<System.IO.Stream>(requestInfo, cancellationToken: cancellationToken).ConfigureAwait(false);
            if (stream is null)
            {
                Capabilities = ServerCapabilities.AssumedCurrent();
                return Capabilities;
            }

            using var reader = new StreamReader(stream);
            body = await reader.ReadToEndAsync(cancellationToken).ConfigureAwait(false);
        }
        catch (ApiException)
        {
            // Liveness has no schema surprises to negotiate over; an erroring liveness
            // probe means the picker's ready check already failed — assume current.
            Capabilities = ServerCapabilities.AssumedCurrent();
            return Capabilities;
        }
        catch (HttpRequestException)
        {
            Capabilities = ServerCapabilities.AssumedCurrent();
            return Capabilities;
        }

        try
        {
            using var json = JsonDocument.Parse(body ?? string.Empty);
            if (json.RootElement.ValueKind == JsonValueKind.Object
                && json.RootElement.TryGetProperty("version", out var versionProperty)
                && versionProperty.ValueKind == JsonValueKind.String
                && versionProperty.GetString() is { Length: > 0 } reported
                && Version.TryParse(reported, out var parsed))
            {
                Capabilities = ServerCapabilities.From(parsed);
                return Capabilities;
            }
        }
        catch (JsonException)
        {
            // A non-JSON liveness answer is treated like a missing version below.
        }

        Capabilities = ServerCapabilities.AssumedCurrent();
        return Capabilities;
    }

    /// <summary>Runs <c>GET /api/health/ready</c>. A 200 or a 503 (draining / failed check) both
    /// answer with the contract's <c>Ready</c> body; unreachable servers return a result
    /// with <c>api.unreachable</c>-style detail instead of throwing.
    /// </summary>
    public async Task<HealthReadyResult> CheckReadyAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        try
        {
            var ready = await Client.Api.Health.Ready.GetAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
            return HealthReadyResult.From(ready, statusCode: 200);
        }
        catch (Ready notReady) when (notReady.ResponseStatusCode == 503)
        {
            // The contract's 503 body is the same Ready type, not the error envelope.
            return HealthReadyResult.From(notReady, statusCode: 503);
        }
        catch (HttpRequestException transportFailure)
        {
            return HealthReadyResult.Unreachable(transportFailure.Message);
        }
        catch (ErrorEnvelope envelope)
        {
            throw RaiseApiError(envelope);
        }
    }

    /// <summary>
    /// Runs <c>POST /api/v1/auth/login</c> and stores the session token in memory only.
    /// A <c>auth.totp_required</c> answer comes back as an <see cref="ApiRequestException"/>
    /// with that code; repeat with <paramref name="totp"/>. The password is never kept or logged.
    /// </summary>
    public async Task<Session> LoginAsync(string user, string password, string? totp = null, CancellationToken cancellationToken = default)
    {
        RequireConnected();
        var body = new Generated.Models.LoginRequest
        {
            User = user,
            Password = password,
            Totp = totp,
        };
        Generated.Api.V1.Auth.Login.LoginPostResponse? response;
        try
        {
            response = await Client.Api.V1.Auth.Login
                .PostAsLoginPostResponseAsync(body, cancellationToken: cancellationToken)
                .ConfigureAwait(false);
        }
        catch (ErrorEnvelope envelope)
        {
            throw RaiseApiError(envelope);
        }

        if (response?.Data?.Token is not { Length: > 0 } token || response.Data.User is null)
        {
            throw new ApiRequestException(ApiError.TransportFailure(0, "The login response carried no session."));
        }

        Tokens.SetToken(token);
        StartExpiryBatch();
        return response.Data;
    }

    // ---- mid-session 401 handling (WP-UI-02.T3 deliverable 2) ----

    /// <summary>
    /// Runs one authorized work call with the production-grade expiry contract: when it
    /// answers 401 <c>auth.session_expired</c>/<c>auth.invalid_credentials</c>,
    /// <see cref="SessionExpired"/> is raised once per expiry batch (concurrent aware
    /// calls share one event and one re-sign-in prompt), the shell's re-sign-in modal is
    /// awaited through <see cref="ReSignInProvider"/>, and a successful prompt retries the
    /// failed request exactly once. A cancelled prompt throws
    /// <see cref="SessionCancelledException"/> carrying the original error. Any other
    /// error, and a retry that fails again, is thrown unchanged to the caller — the shell
    /// keeps every tab, draft and scroll position; nothing is reset.
    /// </summary>
    public async Task<T> RunSessionAwareAsync<T>(Func<Task<T>> call, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(call);
        _awareCallDepth++;
        try
        {
            var generation = Tokens.Generation;
            _callGeneration.Value = generation;
            try
            {
                return await call().ConfigureAwait(false);
            }
            catch (Exception failure) when (SessionBreaking(failure))
            {
                // Hand-written helpers have already cleared the token (RaiseApiError
                // suppresses the hard event inside an aware call); raw generated calls
                // throw the Kiota envelope, so the clearing happens here. Both clear only
                // the token this call was sent under.
                ClearTokenForBreaking(failure);
                // A 401 that arrives after a newer session was installed (another call's
                // prompt finished first) is stale: no second prompt, just the one retry. The
                // prompt repeats that check inside the gate for a call that was beaten to it
                // while it waited here.
                var outcome = Tokens.Generation != generation
                    ? ReSignInOutcome.AlreadyRenewed
                    : await PromptReSignInAsync(generation, cancellationToken).ConfigureAwait(false);
                if (outcome != ReSignInOutcome.Cancelled)
                {
                    // A new session began: its first expiry must raise its own event.
                    StartExpiryBatch();
                    _callGeneration.Value = Tokens.Generation;
                    try
                    {
                        // Exactly one retry: whatever it answers is the caller's outcome.
                        return await call().ConfigureAwait(false);
                    }
                    catch (Exception failureAfter) when (ErrorOf(failureAfter) is { StatusCode: 401 } retryError
                        && Array.IndexOf(SessionBreakingCodes, retryError.Code) >= 0)
                    {
                        // The retried session is also broken: throw the retry's error over
                        // the prompt machinery — the shell shows its message, no prompt.
                        throw new ApiRequestException(retryError, failureAfter);
                    }
                }

                throw new SessionCancelledException(ErrorOf(failure), failure);
            }
        }
        finally
        {
            _awareCallDepth--;
        }
    }

    /// <summary>Clears the token a failed request was sent under; inside an aware call a stale 401 leaves a newer token alone.</summary>
    private void ClearOwnToken()
    {
        if (_callGeneration.Value is { } generation)
        {
            Tokens.ClearTokenIfGeneration(generation);
        }
        else
        {
            Tokens.ClearToken();
        }
    }

    private void ClearTokenForBreaking(Exception failure)
    {
        var code = failure switch
        {
            ApiRequestException api => api.Error.Code,
            ErrorEnvelope envelope => envelope.Error?.Code,
            _ => null,
        };
        if (code is { } breaking && Array.IndexOf(SessionBreakingCodes, breaking) >= 0)
        {
            ClearOwnToken();
        }
    }

    /// <summary>Runs one authorized work call (no result) through the same expiry contract.</summary>
    public async Task RunSessionAwareAsync(Func<Task> call, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(call);
        await RunSessionAwareAsync<object?>(async () => { await call().ConfigureAwait(false); return null; }, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>True when the exception is this session's 401 (either of the breaking codes) on either mapping layer.</summary>
    private static bool SessionBreaking(Exception failure) => failure switch
    {
        ApiRequestException api => api.Error.StatusCode == 401
            && Array.IndexOf(SessionBreakingCodes, api.Error.Code) >= 0,
        ErrorEnvelope envelope => envelope.ResponseStatusCode == 401
            && envelope.Error?.Code is { } code
            && Array.IndexOf(SessionBreakingCodes, code) >= 0,
        _ => false,
    };

    /// <summary>Normalizes both mapping layers (hand-written helpers and raw Kiota envelopes) to an <see cref="ApiError"/>.</summary>
    private static ApiError ErrorOf(Exception failure) => failure switch
    {
        ApiRequestException api => api.Error,
        ErrorEnvelope envelope => ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode),
        _ => ApiError.TransportFailure(0, failure.Message),
    };

    /// <summary>
    /// Awaits the shared re-sign-in prompt for a call whose request was sent under
    /// <paramref name="generation"/>: the first 401 of a batch opens the modal, the concurrent
    /// ones join the same task, and the event fires only for the call that opens it.
    /// The staleness check is repeated <em>inside</em> the gate, because the caller's own check
    /// ran before it started waiting here: a call that finds a newer session was installed in
    /// the meantime (another call's prompt finished) is answered
    /// <see cref="ReSignInOutcome.AlreadyRenewed"/> instead of opening a second modal.
    /// </summary>
    private async Task<ReSignInOutcome> PromptReSignInAsync(long generation, CancellationToken cancellationToken)
    {
        if (BeforeReSignInGateAsync is { } beforeGate)
        {
            await beforeGate().ConfigureAwait(false);
        }

        Task<bool> prompt;
        await _reSignInGate.WaitAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            // Another call's prompt finished while this one waited its turn: the 401 it holds
            // is stale. Do not ask the user to sign in again — the caller retries once.
            if (Tokens.Generation != generation)
            {
                return ReSignInOutcome.AlreadyRenewed;
            }

            if (_reSignInTask is not { } pending || pending.IsCompleted)
            {
                var provider = ReSignInProvider
                    ?? throw new InvalidOperationException("No re-sign-in prompt is wired; the shell must set ReSignInProvider.");
                IsReSignInActive = true;
                prompt = pending = provider();
                _reSignInTask = pending;
                SessionExpired?.Invoke(this, EventArgs.Empty);
            }
            else
            {
                prompt = pending;
            }
        }
        finally
        {
            _reSignInGate.Release();
        }

        try
        {
            return await prompt.ConfigureAwait(false)
                ? ReSignInOutcome.Renewed
                : ReSignInOutcome.Cancelled;
        }
        finally
        {
            if (ReferenceEquals(_reSignInTask, prompt))
            {
                // Cleanup must really take the gate and must not be cancellable: the old form
                // discarded the acquisition and then released a permit it never held, which
                // could let two callers into the gate at once.
                await _reSignInGate.WaitAsync(CancellationToken.None).ConfigureAwait(false);
                try
                {
                    _reSignInTask = null;
                    IsReSignInActive = false;
                }
                finally
                {
                    _reSignInGate.Release();
                }
            }
        }
    }

    /// <summary>What the shared re-sign-in prompt did for one waiting call.</summary>
    private enum ReSignInOutcome
    {
        /// <summary>The user signed in again: the token store holds a newer session.</summary>
        Renewed,

        /// <summary>Another call's prompt installed a newer session first: nothing was prompted.</summary>
        AlreadyRenewed,

        /// <summary>The user cancelled the prompt.</summary>
        Cancelled,
    }

    /// <summary>Runs <c>GET /api/v1/auth/session</c> for the current bearer session.</summary>
    public async Task<SessionInfo> LoadSessionAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        try
        {
            var response = await Client.Api.V1.Auth.Session.GetAsSessionGetResponseAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
            if (response?.Data?.User is null)
            {
                throw new ApiRequestException(ApiError.TransportFailure(0, "The session response carried no user."));
            }

            return response.Data;
        }
        catch (ErrorEnvelope envelope)
        {
            throw RaiseApiError(envelope);
        }
    }

    /// <summary>
    /// Runs <c>POST /api/v1/auth/logout</c> (idempotent). The in-memory token is cleared
    /// even when the server call fails, so the user is always signed out locally; a
    /// remembered token and its hint go with it.
    /// </summary>
    public async Task LogoutAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        if (!Tokens.HasToken)
        {
            await ForgetStoredTokenAsync(cancellationToken).ConfigureAwait(false);
            return;
        }

        try
        {
            await Client.Api.V1.Auth.Logout.PostAsync(cancellationToken: cancellationToken).ConfigureAwait(false);
        }
        catch (Exception exception) when (exception is not OperationCanceledException)
        {
            // Idempotent logout: a failed server call must not keep the user signed in.
            _ = exception;
        }
        finally
        {
            Tokens.ClearToken();
            await ForgetStoredTokenAsync(cancellationToken).ConfigureAwait(false);
            StartExpiryBatch();
        }
    }

    /// <summary>Clears the in-memory token locally: the user left the sign-in modal
    /// cancelled and keeps working read-only, or the shell dropped the session after a
    /// failed start. The stored "Remember me" copy is kept — the user may still resume it.</summary>
    public void DropSessionLocal() => Tokens.ClearToken();

    // ---- change password + my sessions (WP-UI-02.T3 deliverable 4; contract v0.3.0,
    //      WP-CON-03.T4 pending — hand-written request plumbing, switched to the generated
    //      client when the contract lands) ----

    /// <summary>
    /// Runs <c>POST /api/v1/auth/password</c> (contract v0.3.0: the generated client does
    /// not carry it yet). The password travels only in the request body — it is never
    /// stored, logged or included in an error. Policy violations come back as per-field
    /// messages through the contract's error envelope, named under the rules they break.
    /// </summary>
    public async Task ChangePasswordAsync(string currentPassword, string newPassword, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(currentPassword);
        ArgumentException.ThrowIfNullOrWhiteSpace(newPassword);
        RequireConnected();
        using var request = new HttpRequestMessage(HttpMethod.Post, new Uri(BaseUri!, "api/v1/auth/password"))
        {
            Content = new StringContent(
                JsonSerializer.Serialize(new PasswordChangeBody(currentPassword, newPassword), ContractJson),
                System.Text.Encoding.UTF8,
                "application/json"),
        };
        await AttachBearerAsync(request, cancellationToken).ConfigureAwait(false);
        using var response = await SendWithExpiryAsync(request, cancellationToken).ConfigureAwait(false);
        await ThrowUnlessSuccessAsync(response, cancellationToken).ConfigureAwait(false);
    }

    /// <summary>
    /// Runs <c>GET /api/v1/auth/sessions</c> (contract v0.3.0): the signed-in user's
    /// open sessions, the current one marked. Caller gates it behind
    /// <see cref="ServerCapabilities.SessionManagement"/>.
    /// </summary>
    public async Task<IReadOnlyList<ClientSessionSummary>> ListMySessionsAsync(CancellationToken cancellationToken = default)
    {
        RequireConnected();
        using var request = new HttpRequestMessage(HttpMethod.Get, new Uri(BaseUri!, "api/v1/auth/sessions"));
        request.Headers.Accept.ParseAdd("application/json");
        await AttachBearerAsync(request, cancellationToken).ConfigureAwait(false);
        using var response = await SendWithExpiryAsync(request, cancellationToken).ConfigureAwait(false);
        await ThrowUnlessSuccessAsync(response, cancellationToken).ConfigureAwait(false);
        var payload = await response.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            var body = JsonSerializer.Deserialize<SessionListBody>(payload, ContractJson);
            var summaries = body?.Data?.Select(item => item).Where(item => item is not null).ToList() ?? [];
            return summaries!;
        }
        catch (JsonException)
        {
            throw new ApiRequestException(ApiError.TransportFailure((int)response.StatusCode, "The sessions response was not valid."));
        }
    }

    /// <summary>
    /// Runs <c>DELETE /api/v1/auth/sessions/{id}</c> (contract v0.3.0): ends one of the
    /// user's other sessions. Ending the current session is the server's 409; the shell
    /// routes the user to its own "Sign out" (File menu) instead.
    /// </summary>
    public async Task EndMySessionAsync(string sessionId, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(sessionId);
        RequireConnected();
        using var request = new HttpRequestMessage(HttpMethod.Delete, new Uri(BaseUri!, "api/v1/auth/sessions/" + Uri.EscapeDataString(sessionId)));
        await AttachBearerAsync(request, cancellationToken).ConfigureAwait(false);
        using var response = await SendWithExpiryAsync(request, cancellationToken).ConfigureAwait(false);
        await ThrowUnlessSuccessAsync(response, cancellationToken).ConfigureAwait(false);
    }

    private async Task AttachBearerAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var token = await Tokens.GetAuthorizationTokenAsync(BaseUri!, cancellationToken: cancellationToken).ConfigureAwait(false);
        if (token.Length > 0)
        {
            request.Headers.Authorization = new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", token);
        }
    }

    /// <summary>Sends a hand-rolled contract-0.3 request; a session-breaking 401 goes through the same expiry batching and re-sign-in prompt as generated calls.</summary>
    private async Task<HttpResponseMessage> SendWithExpiryAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        // The session this request was stamped with: a 401 that arrives after another call's
        // prompt installed a newer one is stale (same rule as RunSessionAwareAsync).
        var generation = Tokens.Generation;
        _awareCallDepth++;
        try
        {
            using var timeout = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken);
            var response = await _httpClient.SendAsync(request, timeout.Token).ConfigureAwait(false);
            if (response.StatusCode == System.Net.HttpStatusCode.Unauthorized)
            {
                var message = await ReadErrorMessageAsync(response, cancellationToken).ConfigureAwait(false);
                if (Tokens.HasToken && Array.IndexOf(SessionBreakingCodes, message.Code) >= 0)
                {
                    if (Tokens.Generation == generation)
                    {
                        Tokens.ClearTokenIfGeneration(generation);
                        if (Volatile.Read(ref _awareCallDepth) == 1)
                        {
                            RaiseSessionExpiredOnce();
                        }
                    }

                    var outcome = await PromptReSignInAsync(generation, cancellationToken).ConfigureAwait(false);
                    if (outcome == ReSignInOutcome.Cancelled)
                    {
                        throw new SessionCancelledException(message);
                    }

                    // A new session began (either from this prompt or from another call's) and
                    // the hand-rolled request retries once under it.
                    StartExpiryBatch();
                    var retried = await CloneForRetryAsync(request, cancellationToken).ConfigureAwait(false);
                    // The expired attempt's bearer must not ride the retry: take whatever
                    // session is held now.
                    retried.Headers.Authorization = null;
                    await AttachBearerAsync(retried, cancellationToken).ConfigureAwait(false);
                    return await _httpClient.SendAsync(retried, timeout.Token).ConfigureAwait(false);
                }
            }

            return response;
        }
        finally
        {
            _awareCallDepth--;
        }
    }

    private static async Task<HttpRequestMessage> CloneForRetryAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        var clone = new HttpRequestMessage(request.Method, request.RequestUri);
        foreach (var header in request.Headers)
        {
            clone.Headers.TryAddWithoutValidation(header.Key, header.Value);
        }

        if (request.Content is not null)
        {
            var buffer = await request.Content.ReadAsByteArrayAsync(cancellationToken).ConfigureAwait(false);
            clone.Content = new ByteArrayContent(buffer);
            foreach (var header in request.Content.Headers)
            {
                clone.Content.Headers.TryAddWithoutValidation(header.Key, header.Value);
            }
        }

        return clone;
    }

    private static async Task<ApiError> ReadErrorMessageAsync(HttpResponseMessage response, CancellationToken cancellationToken)
    {
        var payload = await response.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
        try
        {
            using var json = JsonDocument.Parse(payload);
            if (json.RootElement.ValueKind == JsonValueKind.Object && json.RootElement.TryGetProperty("error", out var error))
            {
                var code = error.TryGetProperty("code", out var codeProperty) ? codeProperty.GetString() ?? string.Empty : string.Empty;
                var message = error.TryGetProperty("message", out var messageProperty) ? messageProperty.GetString() ?? string.Empty : string.Empty;
                var fields = new List<ApiFieldError>();
                if (error.TryGetProperty("fields", out var fieldsProperty) && fieldsProperty.ValueKind == JsonValueKind.Array)
                {
                    foreach (var field in fieldsProperty.EnumerateArray())
                    {
                        fields.Add(new ApiFieldError(
                            field.TryGetProperty("field", out var fieldProperty) ? fieldProperty.GetString() ?? string.Empty : string.Empty,
                            field.TryGetProperty("code", out var fieldCodeProperty) ? fieldCodeProperty.GetString() ?? string.Empty : string.Empty,
                            field.TryGetProperty("message", out var fieldMessageProperty) ? fieldMessageProperty.GetString() ?? string.Empty : string.Empty));
                    }
                }

                return new ApiError
                {
                    StatusCode = (int)response.StatusCode,
                    Code = code,
                    Message = message,
                    Retryable = error.TryGetProperty("retryable", out var retryableProperty) && retryableProperty.ValueKind == JsonValueKind.True,
                    RequestId = error.TryGetProperty("request_id", out var requestIdProperty) ? requestIdProperty.GetString() : null,
                    FieldErrors = fields,
                };
            }
        }
        catch (JsonException)
        {
            // A non-envelope answer (proxy error page) falls through to the generic error.
        }

        return ApiError.TransportFailure((int)response.StatusCode, "The server answered without a contract error body.");
    }

    private static async Task ThrowUnlessSuccessAsync(HttpResponseMessage response, CancellationToken cancellationToken)
    {
        var status = (int)response.StatusCode;
        if (status is >= 200 and < 300)
        {
            return;
        }

        var error = await ReadErrorMessageAsync(response, cancellationToken).ConfigureAwait(false);
        throw new ApiRequestException(error);
    }

    /// <summary>The request body of <c>POST /api/v1/auth/password</c> (planned contract 0.3.0); rendered snake_case exactly once, never logged.</summary>
    private sealed record PasswordChangeBody(string CurrentPassword, string NewPassword);

    /// <summary>The response envelope of <c>GET /api/v1/auth/sessions</c> (planned contract 0.3.0).</summary>
    private sealed record SessionListBody(IReadOnlyList<ClientSessionSummary>? Data);

    /// <inheritdoc/>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _httpClient.Dispose();
    }

    /// <summary>
    /// Maps the Kiota-thrown <see cref="ErrorEnvelope"/> into an <see cref="ApiRequestException"/>.
    /// A 401 with a session-breaking code clears the token; when the call is not inside
    /// <see cref="RunSessionAwareAsync{T}"/> (which owns the prompt), the hard
    /// <see cref="SessionExpired"/> event is raised exactly once per expiry batch.
    /// </summary>
    private ApiRequestException RaiseApiError(ErrorEnvelope envelope)
    {
        var error = ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode);
        if (error.StatusCode == 401
            && Tokens.HasToken
            && Array.IndexOf(SessionBreakingCodes, error.Code) >= 0)
        {
            ClearOwnToken();
            if (Volatile.Read(ref _awareCallDepth) == 0)
            {
                RaiseSessionExpiredOnce();
            }
        }

        return new ApiRequestException(error);
    }

    /// <summary>Concurrent 401s (hard or aware paths) share one event: a batch resets on the next sign-in.</summary>
    private void RaiseSessionExpiredOnce()
    {
        if (Interlocked.Exchange(ref _expiryEventRaised, 1) == 0)
        {
            SessionExpired?.Invoke(this, EventArgs.Empty);
        }
    }

    /// <summary>The next 401 of a new session raises its own event (a fresh token starts a fresh batch).</summary>
    private void StartExpiryBatch() => Interlocked.Exchange(ref _expiryEventRaised, 0);

    private void RequireConnected()
    {
        if (BaseUri is null)
        {
            throw new InvalidOperationException("Call Connect(baseUri) before using the API.");
        }
    }
}

/// <summary>
/// The outcome of the start-up resume: a remembered session resumed without a form
/// (<see cref="Resumed"/>), a stale token rejected — the shell shows the sign-in step
/// pre-filled (<see cref="Rejected"/>) —, nothing remembered (<see cref="None"/>) or the
/// server unreachable (<see cref="Failed"/>). Never carries a password (none is read).
/// </summary>
/// <param name="Kind">Which outcome this is.</param>
/// <param name="User">The user the token belonged to (prefill on rejection).</param>
/// <param name="Session">The proven session on a successful resume.</param>
/// <param name="Detail">The transport failure's message on <see cref="SessionResumeKind.Failed"/>.</param>
public readonly record struct SessionResumeResult(
    SessionResumeKind Kind,
    string? User,
    SessionInfo? Session,
    string? Detail)
{
    internal static SessionResumeResult None() => new(SessionResumeKind.None, null, null, null);

    internal static SessionResumeResult Resumed(string user, SessionInfo session) =>
        new(SessionResumeKind.Resumed, user, session, null);

    internal static SessionResumeResult Rejected(string user) => new(SessionResumeKind.Rejected, user, null, null);

    internal static SessionResumeResult Failed(string detail) => new(SessionResumeKind.Failed, null, null, detail);
}

/// <summary>The resume outcomes of <see cref="ApiConnection.ResumeSessionAsync"/>.</summary>
public enum SessionResumeKind
{
    /// <summary>No user has a remembered token on this server.</summary>
    None,

    /// <summary>The stored token was valid: straight into the shell.</summary>
    Resumed,

    /// <summary>The stored token was rejected (401): it was deleted and the sign-in step shows, pre-filled.</summary>
    Rejected,

    /// <summary>The server was unreachable during the resume check; the stored token is kept.</summary>
    Failed,
}

/// <summary>
/// The outcome of <c>GET /api/health/ready</c>: whether the site is ready, plus a
/// user-facing reason when it is not. Never throws for an expected not-ready answer.
/// </summary>
/// <param name="IsReady">True only for a 200 with <c>status: ready</c>.</param>
/// <param name="Detail">Failed-check detail or transport message, for the picker's error text.</param>
public readonly record struct HealthReadyResult(bool IsReady, string? Detail)
{
    internal static HealthReadyResult From(Ready? ready, int statusCode)
    {
        if (statusCode == 200 && ready?.Status == Ready_status.Ready)
        {
            return new HealthReadyResult(true, null);
        }

        var failed = ready?.Checks?
            .Where(check => check is not null && check.Ok != true)
            .Select(check => $"{check!.Name}: {check.Detail ?? "failed"}");
        var detail = failed is { } list && list.Any()
            ? string.Join("; ", list)
            : "The server is not ready.";
        return new HealthReadyResult(false, detail);
    }

    internal static HealthReadyResult Unreachable(string message) =>
        new(false, $"Could not reach the server: {message}");
}
