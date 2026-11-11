using System.Text.Json;

namespace Spotledger.AdminCenter;

public enum HealthBadge
{
    Unknown,
    Healthy,
    Degraded,
    Unreachable,
}

/// <summary>
/// A saved connection to a management API. <see cref="PinnedFingerprint"/> is the server
/// certificate fingerprint the administrator trusted on first connect (null until then).
/// No secret is part of a profile; the client certificate is referenced by thumbprint.
/// </summary>
public sealed record ConnectionProfile(
    string Id,
    string Name,
    Uri Endpoint,
    string? ClientCertificateThumbprint = null,
    string? PinnedFingerprint = null,
    string? Group = null,
    IReadOnlyList<string>? Tags = null)
{
    public IReadOnlyList<string> TagList => Tags ?? [];
}

/// <summary>The OS credential store (Windows Credential Manager, macOS Keychain, Secret Service).</summary>
public interface ICredentialStore
{
    Task<string?> ReadAsync(string key, CancellationToken cancellationToken);

    Task WriteAsync(string key, string value, CancellationToken cancellationToken);

    Task DeleteAsync(string key, CancellationToken cancellationToken);
}

/// <summary>What a probe of a management endpoint learned.</summary>
public sealed record ProbeResult(string ServerFingerprint, HealthBadge Health);

public interface IManagementProbe
{
    Task<ProbeResult> ProbeAsync(ConnectionProfile profile, CancellationToken cancellationToken);
}

/// <summary>Asks the administrator whether to trust a first-seen server certificate.</summary>
public interface IPinPrompt
{
    Task<bool> TrustAsync(ConnectionProfile profile, string fingerprint, CancellationToken cancellationToken);
}

public enum ConnectOutcome
{
    Connected,
    PinRejectedByUser,
    PinMismatch,
    Unreachable,
}

public sealed record ConnectResult(ConnectOutcome Outcome, HealthBadge Health);

/// <summary>
/// Adds, edits and removes connections; persists them only through the credential store
/// (never a plain file); runs the certificate-pinning flow on connect.
/// </summary>
public sealed class ConnectionManager
{
    public const string StoreKey = "spotledger.admincenter.connections";

    private static readonly JsonSerializerOptions Json = new() { WriteIndented = false };

    private readonly ICredentialStore _store;
    private readonly Dictionary<string, ConnectionProfile> _profiles = new(StringComparer.Ordinal);
    private readonly Dictionary<string, HealthBadge> _health = new(StringComparer.Ordinal);

    public ConnectionManager(ICredentialStore store)
    {
        ArgumentNullException.ThrowIfNull(store);
        _store = store;
    }

    public IReadOnlyList<ConnectionProfile> Profiles =>
        [.. _profiles.Values.OrderBy(p => p.Name, StringComparer.Ordinal)];

    public HealthBadge HealthOf(string id) => _health.GetValueOrDefault(id, HealthBadge.Unknown);

    public IReadOnlyList<ConnectionProfile> InGroup(string group) =>
        [.. Profiles.Where(p => string.Equals(p.Group, group, StringComparison.Ordinal))];

    public IReadOnlyList<ConnectionProfile> WithTag(string tag) =>
        [.. Profiles.Where(p => p.TagList.Contains(tag, StringComparer.Ordinal))];

    public IReadOnlyList<string> Groups =>
        [.. _profiles.Values.Select(p => p.Group).OfType<string>().Distinct(StringComparer.Ordinal).Order(StringComparer.Ordinal)];

    public async Task LoadAsync(CancellationToken cancellationToken)
    {
        _profiles.Clear();
        var raw = await _store.ReadAsync(StoreKey, cancellationToken).ConfigureAwait(false);
        if (string.IsNullOrEmpty(raw))
        {
            return;
        }

        var stored = JsonSerializer.Deserialize<List<StoredProfile>>(raw, Json) ?? [];
        foreach (var item in stored)
        {
            var loaded = new ConnectionProfile(
                item.Id,
                item.Name,
                new Uri(item.Endpoint),
                item.ClientCertificateThumbprint,
                item.PinnedFingerprint,
                item.Group,
                item.Tags);
            RequireSecureEndpoint(loaded);
            _profiles[item.Id] = loaded;
        }
    }

    /// <summary>Management endpoints must be https: a plain-http endpoint is refused (fail closed).</summary>
    private static void RequireSecureEndpoint(ConnectionProfile profile)
    {
        if (!profile.Endpoint.IsAbsoluteUri
            || !string.Equals(profile.Endpoint.Scheme, Uri.UriSchemeHttps, StringComparison.OrdinalIgnoreCase))
        {
            throw new ArgumentException("A management endpoint must be an absolute https address.", nameof(profile));
        }
    }

    public Task AddAsync(ConnectionProfile profile, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(profile);
        RequireSecureEndpoint(profile);
        if (_profiles.ContainsKey(profile.Id))
        {
            throw new InvalidOperationException($"A connection with id '{profile.Id}' already exists.");
        }

        _profiles[profile.Id] = profile;
        return SaveAsync(cancellationToken);
    }

    /// <summary>Replaces a profile. Changing the endpoint drops the pin: the new host must be trusted again.</summary>
    public Task UpdateAsync(ConnectionProfile profile, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(profile);
        RequireSecureEndpoint(profile);
        if (!_profiles.TryGetValue(profile.Id, out var existing))
        {
            throw new KeyNotFoundException($"No connection with id '{profile.Id}'.");
        }

        _profiles[profile.Id] = existing.Endpoint == profile.Endpoint ? profile : profile with { PinnedFingerprint = null };
        return SaveAsync(cancellationToken);
    }

    public Task RemoveAsync(string id, CancellationToken cancellationToken)
    {
        _profiles.Remove(id);
        _health.Remove(id);
        return SaveAsync(cancellationToken);
    }

    /// <summary>
    /// First connect: probe, ask the administrator to trust the fingerprint, then pin it.
    /// Later connects: a different fingerprint is refused without prompting.
    /// </summary>
    public async Task<ConnectResult> ConnectAsync(
        string id,
        IManagementProbe probe,
        IPinPrompt prompt,
        CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(probe);
        ArgumentNullException.ThrowIfNull(prompt);
        var profile = _profiles.TryGetValue(id, out var found) ? found : throw new KeyNotFoundException($"No connection with id '{id}'.");

        ProbeResult result;
        try
        {
            result = await probe.ProbeAsync(profile, cancellationToken).ConfigureAwait(false);
        }
        catch (HttpRequestException)
        {
            _health[id] = HealthBadge.Unreachable;
            return new ConnectResult(ConnectOutcome.Unreachable, HealthBadge.Unreachable);
        }

        if (profile.PinnedFingerprint is null)
        {
            if (!await prompt.TrustAsync(profile, result.ServerFingerprint, cancellationToken).ConfigureAwait(false))
            {
                return new ConnectResult(ConnectOutcome.PinRejectedByUser, HealthOf(id));
            }

            _profiles[id] = profile with { PinnedFingerprint = result.ServerFingerprint };
            await SaveAsync(cancellationToken).ConfigureAwait(false);
        }
        else if (!string.Equals(profile.PinnedFingerprint, result.ServerFingerprint, StringComparison.Ordinal))
        {
            _health[id] = HealthBadge.Unreachable;
            return new ConnectResult(ConnectOutcome.PinMismatch, HealthBadge.Unreachable);
        }

        _health[id] = result.Health;
        return new ConnectResult(ConnectOutcome.Connected, result.Health);
    }

    private Task SaveAsync(CancellationToken cancellationToken)
    {
        var stored = _profiles.Values
            .OrderBy(p => p.Id, StringComparer.Ordinal)
            .Select(p => new StoredProfile(
                p.Id, p.Name, p.Endpoint.AbsoluteUri, p.ClientCertificateThumbprint, p.PinnedFingerprint, p.Group, p.Tags))
            .ToList();
        return _store.WriteAsync(StoreKey, JsonSerializer.Serialize(stored, Json), cancellationToken);
    }

    private sealed record StoredProfile(
        string Id,
        string Name,
        string Endpoint,
        string? ClientCertificateThumbprint,
        string? PinnedFingerprint,
        string? Group,
        IReadOnlyList<string>? Tags);
}
