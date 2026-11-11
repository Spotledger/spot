using System.Collections.Concurrent;

namespace Spotledger.Client.Api;

/// <summary>
/// The test fake of <see cref="ISecretStore"/> and the fallback when the OS store is
/// unavailable: a plain dictionary. Instances expose what was written so tests can assert
/// a password never lands here and only token values do.
/// </summary>
public sealed class InMemorySecretStore : ISecretStore
{
    private readonly ConcurrentDictionary<string, string> _secrets = new(StringComparer.Ordinal);

    /// <summary>Secrets written so far (key, value) in call order — for the tests' assertions.</summary>
    public IReadOnlyList<(string Key, string Value)> Writes =>
        _writes.ToList();

    private readonly List<(string Key, string Value)> _writes = new();

    /// <summary>Keys removed so far, in call order.</summary>
    public IReadOnlyList<string> Deletes =>
        _deletes.ToList();

    private readonly List<string> _deletes = new();

    public Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        return _secrets.TryGetValue(key, out var value)
            ? Task.FromResult<string?>(value)
            : Task.FromResult<string?>(null);
    }

    public Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        ArgumentException.ThrowIfNullOrWhiteSpace(value);
        _writes.Add((key, value));
        _secrets[key] = value;
        return Task.CompletedTask;
    }

    public Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        _deletes.Add(key);
        _ = _secrets.TryRemove(key, out _);
        return Task.CompletedTask;
    }
}