using Microsoft.Kiota.Abstractions.Authentication;

namespace Spotledger.Client.Api;

/// <summary>
/// Holds the current session token in memory and feeds it to the Kiota bearer provider,
/// so every request after login carries <c>Authorization: Bearer …</c> and requests
/// before it carry none (the contract's login and health endpoints are unsecured).
/// Redaction (WP-UI-02.T3): <see cref="ToString"/> reports presence only, so no view
/// model, log line or exception message can render the token itself — the value flows
/// only to the HTTP header and the OS secret store.
/// </summary>
public sealed class SessionTokenStore : IAccessTokenProvider
{
    private readonly object _gate = new();
    private string? _token;
    private long _generation;

    /// <summary>True when a session token is held (i.e. after login).</summary>
    public bool HasToken => Volatile.Read(ref _token) is not null;

    /// <summary>
    /// Counts the tokens installed so far. A request remembers the generation it was sent
    /// under, so a 401 that arrives after a newer token was set is recognised as stale
    /// and must not clear the new session.
    /// </summary>
    public long Generation => Volatile.Read(ref _generation);

    /// <summary>Stores the session token returned by <c>POST /api/v1/auth/login</c>.</summary>
    /// <param name="token">The opaque session token from the contract's <c>Session</c>.</param>
    public void SetToken(string token)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(token);
        lock (_gate)
        {
            _token = token;
            _generation++;
        }
    }

    /// <summary>Drops the in-memory session token; requests stop carrying the header.</summary>
    public void ClearToken()
    {
        lock (_gate)
        {
            _token = null;
        }
    }

    /// <summary>Drops the token only if it is still the one installed at <paramref name="generation"/>.</summary>
    public void ClearTokenIfGeneration(long generation)
    {
        lock (_gate)
        {
            if (_generation == generation)
            {
                _token = null;
            }
        }
    }

    /// <summary>
    /// The token for the one persistence consumer ("Remember me"'s keyring write).
    /// Null when no token is held. Not a display surface: the value goes only to the OS
    /// credential store and the HTTP header, nowhere else.
    /// </summary>
    public string? GetTokenForSecretStore() => Volatile.Read(ref _token);

    /// <inheritdoc/>
    public Task<string> GetAuthorizationTokenAsync(
        Uri uri,
        Dictionary<string, object>? additionalAuthenticationContext = null,
        CancellationToken cancellationToken = default)
    {
        // No token means "no credentials": the bearer provider skips the header when the
        // value is empty, which is correct before login and for unsecured endpoints.
        return Task.FromResult(Volatile.Read(ref _token) ?? string.Empty);
    }

    /// <inheritdoc/>
    public AllowedHostsValidator AllowedHostsValidator { get; } = new();

    /// <summary>Presence only — the token value itself must never render anywhere.</summary>
    public override string ToString() =>
        HasToken ? "SessionTokenStore(1 token held)" : "SessionTokenStore(empty)";
}
