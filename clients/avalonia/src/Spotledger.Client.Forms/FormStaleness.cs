using System.ComponentModel;
using Spotledger.Client.Notifications;

namespace Spotledger.Client.Forms;

/// <summary>
/// An open form as the staleness watcher sees it (WP-UI-02.T6): the document's identity, the version the
/// form loaded or last saved, whether it has unsaved edits, and a reload. Everything is read live, so the
/// watcher judges each realtime event against the form as it is at that moment.
/// </summary>
/// <param name="form">The form.</param>
public sealed class WatchedForm(DocumentFormViewModel form) : IWatchedDocument
{
    /// <inheritdoc/>
    public string DocType => form.DocType;

    /// <inheritdoc/>
    public string Id => form.DocumentId ?? string.Empty;

    /// <inheritdoc/>
    public int LoadedVersion => form.Draft?.Version ?? 0;

    /// <inheritdoc/>
    public bool IsDirty => form.IsDirty;

    /// <inheritdoc/>
    public Task ReloadAsync() => form.ReloadAsync();
}

/// <summary>
/// The staleness of one open form tab: one watcher join once the form is loaded and has an id (a new
/// document joins when it is first saved), the <see cref="StalenessBar"/> in the form's
/// <see cref="DocumentFormViewModel.Banner"/> slot, and a leave when disposed (tab closed). A dirty form is
/// offered a merge of its edits with the server's version besides "Reload (discard my changes)".
/// </summary>
public sealed class FormStaleness : IDisposable
{
    private readonly DocumentFormViewModel _form;
    private readonly DocumentStalenessWatcher _watcher;
    private DocumentWatch? _watch;
    private StalenessBar? _bar;
    private bool _disposed;

    /// <summary>Starts watching <paramref name="form"/>. Call on the UI thread (it builds the bar).</summary>
    public FormStaleness(DocumentFormViewModel form, DocumentStalenessWatcher watcher)
    {
        _form = form ?? throw new ArgumentNullException(nameof(form));
        _watcher = watcher ?? throw new ArgumentNullException(nameof(watcher));
        _form.PropertyChanged += OnFormChanged;
        TryJoin();
    }

    /// <summary>The form's watch; null until the form is loaded and saved (has an id).</summary>
    public DocumentWatch? Watch => _watch;

    /// <summary>The bar shown above the form; null until the watch exists.</summary>
    public StalenessBar? Bar => _bar;

    /// <summary>Leaves the document's room and takes the bar away.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _form.PropertyChanged -= OnFormChanged;
        if (ReferenceEquals(_form.Banner, _bar))
        {
            _form.Banner = null;
        }

        _bar?.Dispose();
        _watch?.Dispose();
        _bar = null;
        _watch = null;
    }

    private void OnFormChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName is nameof(DocumentFormViewModel.DocumentId) or nameof(DocumentFormViewModel.Phase))
        {
            TryJoin();
        }
    }

    private void TryJoin()
    {
        if (_disposed || _watch is not null || _form.Phase != FormPhase.Ready || _form.DocumentId is null)
        {
            return;
        }

        _watch = _watcher.Watch(new WatchedForm(_form));
        _bar = new StalenessBar(
            _watch,
            () => _form.Shell.ConfirmDiscardAsync(_form.Draft?.DirtyPaths() ?? []),
            MergeAsync);
        _form.Banner = _bar;
    }

    private async Task MergeAsync()
    {
        if (await _form.MergeServerChangesAsync())
        {
            _watch?.Clear();
        }
    }
}
