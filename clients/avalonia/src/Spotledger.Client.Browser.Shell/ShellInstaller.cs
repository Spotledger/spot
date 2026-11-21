namespace Spotledger.Client.Browser.Shell;

/// <summary>What <see cref="ShellInstaller.StageAsync"/> did.</summary>
/// <param name="Plan">The plan it followed.</param>
/// <param name="Downloaded">How many files it downloaded.</param>
/// <param name="Activated">True when the build became the active one at once (nothing was running from the cache).</param>
public sealed record ShellStageResult(ShellCachePlan Plan, int Downloaded, bool Activated);

/// <summary>
/// Moves the cache to a new build in two steps so a running page never loses a file it may still load.
/// <see cref="StageAsync"/> downloads what the planner says, checks each file against its manifest hash and
/// writes a <c>pending</c> record; nothing the running build needs is deleted. <see cref="ActivateAsync"/>
/// (on the next load, or when the user applies the update) makes the pending record the active one and only
/// then removes the keys no longer needed. A hash mismatch throws and leaves the cache as it was.
/// </summary>
public sealed class ShellInstaller
{
    /// <summary>The name of the active version record.</summary>
    public const string ActiveRecord = "active";

    /// <summary>The name of the staged version record.</summary>
    public const string PendingRecord = "pending";

    /// <summary>The key prefix of shell files in the storage (hash-keyed documents use another prefix).</summary>
    public const string KeyPrefix = "shell/";

    private readonly IShellStorage _storage;
    private readonly IShellNetwork _network;
    private readonly IContentHasher _hasher;

    /// <summary>Creates the installer.</summary>
    public ShellInstaller(IShellStorage storage, IShellNetwork network, IContentHasher? hasher = null)
    {
        _storage = storage;
        _network = network;
        _hasher = hasher ?? new Sha256ContentHasher();
    }

    /// <summary>The active record, or null.</summary>
    public async Task<ShellCacheRecord?> ReadActiveAsync(CancellationToken cancellationToken = default) =>
        ShellCacheRecord.TryParse(await _storage.ReadRecordAsync(ActiveRecord, cancellationToken).ConfigureAwait(false));

    /// <summary>The staged record, or null.</summary>
    public async Task<ShellCacheRecord?> ReadPendingAsync(CancellationToken cancellationToken = default) =>
        ShellCacheRecord.TryParse(await _storage.ReadRecordAsync(PendingRecord, cancellationToken).ConfigureAwait(false));

    /// <summary>Downloads and verifies what <paramref name="next"/> needs. Activates at once on a cold start.</summary>
    /// <exception cref="ShellIntegrityException">A downloaded file did not match its hash; nothing was committed.</exception>
    /// <exception cref="ShellNetworkException">A download failed; nothing was committed.</exception>
    public async Task<ShellStageResult> StageAsync(ShellManifest next, string contractVersion, CancellationToken cancellationToken = default)
    {
        var active = await ReadActiveAsync(cancellationToken).ConfigureAwait(false);
        var present = await PresentKeysAsync(cancellationToken).ConfigureAwait(false);
        var plan = ShellCachePlanner.Plan(active, present, next, contractVersion);

        var written = new List<string>();
        try
        {
            foreach (var entry in plan.Fetch)
            {
                var bytes = await _network.FetchAsync(entry.Path, cancellationToken).ConfigureAwait(false);
                if (bytes.LongLength != entry.Bytes || !string.Equals(_hasher.Sha256Hex(bytes), entry.Sha256, StringComparison.Ordinal))
                {
                    throw new ShellIntegrityException("The downloaded file " + entry.Path + " does not match the asset manifest.");
                }

                await _storage.PutAsync(KeyPrefix + entry.Key, bytes, cancellationToken).ConfigureAwait(false);
                written.Add(entry.Key);
            }
        }
        catch
        {
            // Roll back only what this run added and nothing else needs (a key kept by the active build stays).
            var activeKeys = new HashSet<string>(active?.Keys ?? [], StringComparer.Ordinal);
            foreach (string key in written.Where(k => !activeKeys.Contains(k) && !present.Contains(k)))
            {
                await _storage.DeleteAsync(KeyPrefix + key, CancellationToken.None).ConfigureAwait(false);
            }

            throw;
        }

        var record = new ShellCacheRecord(
            next.Build,
            contractVersion,
            next.CacheableEntries.Select(e => e.Key).OrderBy(k => k, StringComparer.Ordinal).ToList());
        await _storage.WriteRecordAsync(PendingRecord, record.ToJson(), cancellationToken).ConfigureAwait(false);

        bool activated = false;
        if (active is null)
        {
            await ActivateAsync(cancellationToken).ConfigureAwait(false);
            activated = true;
        }

        return new ShellStageResult(plan, plan.Fetch.Count, activated);
    }

    /// <summary>
    /// Makes the staged build the active one and removes the keys it does not need. Returns false when nothing
    /// was staged. If a staged key has been evicted meanwhile, the staged record is dropped and false is returned
    /// (the next <see cref="StageAsync"/> repairs it) rather than activating a build that is not whole.
    /// </summary>
    public async Task<bool> ActivateAsync(CancellationToken cancellationToken = default)
    {
        var pending = await ReadPendingAsync(cancellationToken).ConfigureAwait(false);
        if (pending is null)
        {
            return false;
        }

        var present = new HashSet<string>(await PresentKeysAsync(cancellationToken).ConfigureAwait(false), StringComparer.Ordinal);
        if (pending.Keys.Any(k => !present.Contains(k)))
        {
            await _storage.DeleteRecordAsync(PendingRecord, cancellationToken).ConfigureAwait(false);
            return false;
        }

        await _storage.WriteRecordAsync(ActiveRecord, pending.ToJson(), cancellationToken).ConfigureAwait(false);
        await _storage.DeleteRecordAsync(PendingRecord, cancellationToken).ConfigureAwait(false);
        var needed = new HashSet<string>(pending.Keys, StringComparer.Ordinal);
        foreach (string key in present.Where(k => !needed.Contains(k)))
        {
            await _storage.DeleteAsync(KeyPrefix + key, cancellationToken).ConfigureAwait(false);
        }

        return true;
    }

    /// <summary>The shell keys in the storage, without the prefix.</summary>
    public async Task<IReadOnlyList<string>> PresentKeysAsync(CancellationToken cancellationToken = default)
    {
        var keys = await _storage.ListKeysAsync(KeyPrefix, cancellationToken).ConfigureAwait(false);
        return keys.Select(k => k[KeyPrefix.Length..]).ToList();
    }

    /// <summary>The cached bytes of a file of the active build, or null (not part of the build, or evicted).</summary>
    public async Task<byte[]?> ReadActiveFileAsync(string path, CancellationToken cancellationToken = default)
    {
        var active = await ReadActiveAsync(cancellationToken).ConfigureAwait(false);
        string? key = active?.Keys.FirstOrDefault(k => string.Equals(ShellEntry.PathOfKey(k), path, StringComparison.Ordinal));
        if (key is null)
        {
            return null;
        }

        var bytes = await _storage.GetAsync(KeyPrefix + key, cancellationToken).ConfigureAwait(false);
        // Fail closed: a cached file whose content no longer matches its key is treated as absent.
        return bytes is not null && string.Equals(_hasher.Sha256Hex(bytes), key[(key.LastIndexOf('#') + 1)..], StringComparison.Ordinal)
            ? bytes
            : null;
    }
}
