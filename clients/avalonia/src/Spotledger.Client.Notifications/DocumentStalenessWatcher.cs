using System.Text.Json;
using Spotledger.Client.Realtime;

namespace Spotledger.Client.Notifications;

/// <summary>What the staleness bar of one open form shows.</summary>
public enum StalenessKind
{
    /// <summary>Nothing: the form is current as far as anyone has said.</summary>
    None = 0,

    /// <summary>Someone else changed the document and the form is clean: offer "Reload".</summary>
    ChangedClean = 1,

    /// <summary>Someone else changed the document and the form has unsaved edits: reload discards them.</summary>
    ChangedDirty = 2,

    /// <summary>The document was cancelled by someone.</summary>
    Cancelled = 3,

    /// <summary>The document was deleted by someone.</summary>
    Deleted = 4,

    /// <summary>The server could not replay what was missed: the form may be out of date.</summary>
    MaybeStale = 5,
}

/// <summary>The open form, as the watcher needs to see it. Read live at every event.</summary>
public interface IWatchedDocument
{
    /// <summary>The DocType of the form.</summary>
    string DocType { get; }

    /// <summary>The document id.</summary>
    string Id { get; }

    /// <summary>The version the form loaded or last saved.</summary>
    int LoadedVersion { get; }

    /// <summary>Whether the form has unsaved changes.</summary>
    bool IsDirty { get; }

    /// <summary>Reloads the document from the server into the form.</summary>
    Task ReloadAsync();
}

/// <summary>
/// Joins <c>doc:&lt;doctype&gt;:&lt;id&gt;</c> for each open form tab and leaves on close
/// (<see cref="ActiveWatches"/> equals the open tabs). Events are hints: a version the form already
/// holds is ignored, which also swallows the echo of this user's own save, and nothing reloads under
/// the person's eyes unless the "reload clean forms" setting is on.
/// </summary>
public sealed class DocumentStalenessWatcher(IRoomSource rooms, Func<bool>? autoReloadClean = null)
{
    private int _active;

    /// <summary>How many forms are being watched now.</summary>
    public int ActiveWatches => Volatile.Read(ref _active);

    /// <summary>Starts watching one open form; dispose the result when its tab closes.</summary>
    public DocumentWatch Watch(IWatchedDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        Interlocked.Increment(ref _active);
        return new DocumentWatch(rooms, document, autoReloadClean ?? (() => false), () => Interlocked.Decrement(ref _active));
    }
}

/// <summary>The staleness of one open form.</summary>
public sealed class DocumentWatch : IDisposable
{
    private readonly object _gate = new();
    private readonly IWatchedDocument _document;
    private readonly Func<bool> _autoReloadClean;
    private readonly Action _onDispose;
    private readonly IRoomSubscription _subscription;
    private StalenessKind _kind;
    private bool _disposed;

    internal DocumentWatch(IRoomSource rooms, IWatchedDocument document, Func<bool> autoReloadClean, Action onDispose)
    {
        _document = document;
        _autoReloadClean = autoReloadClean;
        _onDispose = onDispose;
        Room = $"doc:{document.DocType}:{document.Id}";
        _subscription = rooms.Join(Room, OnMessage);
    }

    /// <summary>The room joined.</summary>
    public string Room { get; }

    /// <summary>What the bar shows now.</summary>
    public StalenessKind Kind
    {
        get
        {
            lock (_gate)
            {
                return _kind;
            }
        }
    }

    /// <summary>Raised when <see cref="Kind"/> changed (any thread).</summary>
    public event EventHandler? Changed;

    /// <summary>
    /// Reloads the form. A dirty form asks <paramref name="confirmDiscard"/> first and does nothing if the
    /// answer is no. Returns whether the reload happened.
    /// </summary>
    public async Task<bool> ReloadAsync(Func<Task<bool>>? confirmDiscard = null)
    {
        if (_document.IsDirty && (confirmDiscard is null || !await confirmDiscard().ConfigureAwait(true)))
        {
            return false;
        }

        await _document.ReloadAsync().ConfigureAwait(true);
        Set(StalenessKind.None);
        return true;
    }

    /// <summary>Clears the bar (the form was reloaded by other means).</summary>
    public void Clear() => Set(StalenessKind.None);

    /// <summary>Leaves the room.</summary>
    public void Dispose()
    {
        lock (_gate)
        {
            if (_disposed)
            {
                return;
            }

            _disposed = true;
        }

        _subscription.Dispose();
        _onDispose();
    }

    private void OnMessage(RealtimeRoomMessage message)
    {
        switch (message)
        {
            case RealtimeEvent realtimeEvent:
                OnEvent(realtimeEvent);
                break;
            case RealtimeResyncRequired:
                Raise(StalenessKind.MaybeStale);
                _subscription.AcknowledgeResync();
                break;
        }
    }

    private void OnEvent(RealtimeEvent realtimeEvent)
    {
        if (realtimeEvent.Payload.ValueKind != JsonValueKind.Object
            || !Matches(realtimeEvent.Payload)
            || !realtimeEvent.Payload.TryGetProperty("version", out var versionElement)
            || !versionElement.TryGetInt32(out var version))
        {
            return;
        }

        switch (realtimeEvent.Kind)
        {
            case RealtimeEventKind.DocDeleted:
                Raise(StalenessKind.Deleted);
                return;
            case RealtimeEventKind.DocUpdated or RealtimeEventKind.DocPosted or RealtimeEventKind.DocRenamed or RealtimeEventKind.DocCancelled:
                if (version <= _document.LoadedVersion)
                {
                    return;
                }

                if (realtimeEvent.Kind == RealtimeEventKind.DocCancelled)
                {
                    Raise(StalenessKind.Cancelled);
                }
                else
                {
                    OnChanged(_document.IsDirty);
                }

                return;
        }
    }

    private void OnChanged(bool dirty)
    {
        if (!dirty && _autoReloadClean())
        {
            _ = ReloadQuietlyAsync();
            return;
        }

        Raise(dirty ? StalenessKind.ChangedDirty : StalenessKind.ChangedClean);
    }

    private async Task ReloadQuietlyAsync()
    {
        try
        {
            await _document.ReloadAsync().ConfigureAwait(false);
            Set(StalenessKind.None);
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            // The reload failed: fall back to offering it.
            Raise(StalenessKind.ChangedClean);
        }
    }

    private bool Matches(JsonElement payload) =>
        payload.TryGetProperty("doctype", out var doctype) && doctype.ValueKind == JsonValueKind.String && doctype.GetString() == _document.DocType
        && payload.TryGetProperty("id", out var id) && id.ValueKind == JsonValueKind.String && id.GetString() == _document.Id;

    private void Raise(StalenessKind kind)
    {
        // Deleted and cancelled are final; a later "changed" never hides them.
        lock (_gate)
        {
            if (_kind is StalenessKind.Deleted || (_kind is StalenessKind.Cancelled && kind is not StalenessKind.Deleted))
            {
                return;
            }
        }

        Set(kind);
    }

    private void Set(StalenessKind kind)
    {
        lock (_gate)
        {
            if (_disposed || _kind == kind)
            {
                return;
            }

            _kind = kind;
        }

        Changed?.Invoke(this, EventArgs.Empty);
    }
}
