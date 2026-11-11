using System.Collections.Concurrent;

namespace Spotledger.Client.Workspace;

/// <summary>
/// A draft store that lives in memory: the fallback when no key can be kept in the OS credential store,
/// and the test double. Records are held as they are given (they are immutable) and are lost with the process.
/// </summary>
public sealed class InMemoryDraftStore : IDraftStore
{
    private readonly ConcurrentDictionary<string, DraftRecord> _records = new(StringComparer.Ordinal);
    private readonly List<QuarantineNote> _quarantined = [];

    /// <inheritdoc />
    public string? MemoryOnlyNotice => FileDraftStore.MemoryOnlyMessage;

    /// <inheritdoc />
    public IReadOnlyList<QuarantineNote> Quarantined => _quarantined;

    /// <inheritdoc />
    public event EventHandler<QuarantineNote>? DraftQuarantined
    {
        add { }
        remove { }
    }

    /// <inheritdoc />
    public Task<bool> IsPersistentAsync(DraftScope scope, CancellationToken cancellationToken = default) => Task.FromResult(false);

    /// <inheritdoc />
    public Task<DraftSaveResult> SaveAsync(DraftRecord record, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(record);
        _records[Id(record.Key)] = record;
        return Task.FromResult(new DraftSaveResult(DraftSaveOutcome.SavedInMemoryOnly));
    }

    /// <inheritdoc />
    public Task<DraftRecord?> LoadAsync(DraftKey key, CancellationToken cancellationToken = default)
        => Task.FromResult(_records.TryGetValue(Id(key), out var record) ? record : null);

    /// <inheritdoc />
    public Task DeleteAsync(DraftKey key, CancellationToken cancellationToken = default)
    {
        _ = _records.TryRemove(Id(key), out _);
        return Task.CompletedTask;
    }

    /// <inheritdoc />
    public Task<IReadOnlyList<DraftSummary>> ListAsync(DraftScope scope, CancellationToken cancellationToken = default)
    {
        var prefix = scope.DirectoryName + "/";
        IReadOnlyList<DraftSummary> list =
        [
            .. _records
                .Where(pair => pair.Key.StartsWith(prefix, StringComparison.Ordinal))
                .Select(pair => new DraftSummary(pair.Value.Key, pair.Value.Title, pair.Value.SavedAtUtc, pair.Value.ChangedCount))
                .OrderByDescending(s => s.SavedAtUtc),
        ];
        return Task.FromResult(list);
    }

    /// <inheritdoc />
    public Task DeleteAllAsync(DraftScope scope, CancellationToken cancellationToken = default)
    {
        var prefix = scope.DirectoryName + "/";
        foreach (var id in _records.Keys.Where(k => k.StartsWith(prefix, StringComparison.Ordinal)).ToList())
        {
            _ = _records.TryRemove(id, out _);
        }

        return Task.CompletedTask;
    }

    private static string Id(DraftKey key) => key.Scope.DirectoryName + "/" + key.FileName;
}
