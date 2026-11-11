namespace Spotledger.Client.Browser.Shell;

/// <summary>What a form or list view shows.</summary>
public enum ViewLoadState
{
    /// <summary>Waiting for the module, the metadata or the view definition: show the skeleton.</summary>
    Skeleton,

    /// <summary>Everything arrived: show the view.</summary>
    Ready,

    /// <summary>Something failed: show <see cref="ViewLoadTracker.Error"/> and offer a retry.</summary>
    Failed,
}

/// <summary>
/// Tracks the three things a form or list view waits for (its module, the DocType metadata and the view
/// definition) and says whether to show the skeleton. It holds no data, only which parts arrived.
/// </summary>
public sealed class ViewLoadTracker
{
    private bool _module;
    private bool _metadata;
    private bool _definition;

    /// <summary>The state to show.</summary>
    public ViewLoadState State { get; private set; } = ViewLoadState.Skeleton;

    /// <summary>The plain error message while <see cref="State"/> is Failed; otherwise null.</summary>
    public string? Error { get; private set; }

    /// <summary>True while the skeleton shows.</summary>
    public bool ShowSkeleton => State == ViewLoadState.Skeleton;

    /// <summary>The module is loaded.</summary>
    public void ModuleLoaded() => Arrived(ref _module);

    /// <summary>The metadata arrived.</summary>
    public void MetadataArrived() => Arrived(ref _metadata);

    /// <summary>The view definition arrived.</summary>
    public void DefinitionArrived() => Arrived(ref _definition);

    /// <summary>A part failed; the view shows the error until <see cref="Retry"/>.</summary>
    public void Fail(string error)
    {
        Error = error;
        State = ViewLoadState.Failed;
    }

    /// <summary>Goes back to the skeleton to try again. Parts that already arrived stay arrived.</summary>
    public void Retry()
    {
        if (State == ViewLoadState.Failed)
        {
            Error = null;
            State = _module && _metadata && _definition ? ViewLoadState.Ready : ViewLoadState.Skeleton;
        }
    }

    private void Arrived(ref bool part)
    {
        part = true;
        if (State == ViewLoadState.Skeleton && _module && _metadata && _definition)
        {
            State = ViewLoadState.Ready;
        }
    }
}

/// <summary>
/// A small persistent cache for metadata and view-definition documents, keyed by the document's content hash.
/// A document is stored only under the hash of its own bytes and is checked again on every read, so a wrong or
/// damaged entry is never returned. Only these two kinds are accepted: a data response is never cached here.
/// </summary>
public sealed class HashedDocumentCache
{
    /// <summary>The kind of a DocType metadata document.</summary>
    public const string Metadata = "metadata";

    /// <summary>The kind of a view definition.</summary>
    public const string View = "view";

    private const string Prefix = "doc/";

    private readonly IShellStorage _storage;
    private readonly IContentHasher _hasher;

    /// <summary>Creates the cache.</summary>
    public HashedDocumentCache(IShellStorage storage, IContentHasher? hasher = null)
    {
        _storage = storage;
        _hasher = hasher ?? new Sha256ContentHasher();
    }

    /// <summary>Stores <paramref name="bytes"/> under its own hash and returns the hash; throws for another kind.</summary>
    /// <exception cref="ArgumentException">The kind is not metadata or view.</exception>
    public async Task<string> PutAsync(string kind, byte[] bytes, CancellationToken cancellationToken = default)
    {
        RequireKind(kind);
        string hash = _hasher.Sha256Hex(bytes);
        await _storage.PutAsync(Key(kind, hash), bytes, cancellationToken).ConfigureAwait(false);
        return hash;
    }

    /// <summary>The cached document with this hash, or null when absent or no longer matching its hash.</summary>
    public async Task<byte[]?> GetAsync(string kind, string hash, CancellationToken cancellationToken = default)
    {
        RequireKind(kind);
        if (!ShellManifest.IsHash(hash))
        {
            return null;
        }

        var bytes = await _storage.GetAsync(Key(kind, hash), cancellationToken).ConfigureAwait(false);
        if (bytes is null)
        {
            return null;
        }

        if (!string.Equals(_hasher.Sha256Hex(bytes), hash, StringComparison.Ordinal))
        {
            await _storage.DeleteAsync(Key(kind, hash), cancellationToken).ConfigureAwait(false);
            return null;
        }

        return bytes;
    }

    private static string Key(string kind, string hash) => Prefix + kind + "/" + hash;

    private static void RequireKind(string kind)
    {
        if (kind is not (Metadata or View))
        {
            throw new ArgumentException("Only metadata and view documents are cached.", nameof(kind));
        }
    }
}
