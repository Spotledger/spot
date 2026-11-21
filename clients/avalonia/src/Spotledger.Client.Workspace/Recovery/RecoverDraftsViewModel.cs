using System.Collections.ObjectModel;
using System.Globalization;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Workspace;

/// <summary>One row of the "Recover drafts" list.</summary>
/// <param name="Summary">The stored draft's summary.</param>
/// <param name="Document">What the draft is for: "Invoice INV-1" or "New Invoice".</param>
/// <param name="TimeText">When it was written, formatted.</param>
public sealed record RecoverDraftItem(DraftSummary Summary, string Document, string TimeText)
{
    /// <summary>The draft's key.</summary>
    public DraftKey Key => Summary.Key;
}

/// <summary>
/// The "Recover drafts" list: every draft on this machine for the signed-in user and server, newest
/// first, shown after a crash or kill (and any time from the File menu). Each opens on its own (the shell
/// opens the document or a new document, whose form then offers the restore bar) or is discarded.
/// Drafts of other users and other servers are not here.
/// </summary>
public sealed class RecoverDraftsViewModel : ObservableObject
{
    private readonly IDraftStore _store;
    private readonly DraftScope _scope;
    private readonly Func<DraftKey, Task> _open;
    private readonly Func<DateTimeOffset, string> _formatTime;
    private readonly Func<string, string> _docTypeLabel;

    /// <summary>Creates the list.</summary>
    /// <param name="store">The draft store.</param>
    /// <param name="scope">The signed-in user and server.</param>
    /// <param name="open">Opens the document of a draft in a tab (the shell supplies this).</param>
    /// <param name="docTypeLabel">The display label of a DocType (the DocType name when absent).</param>
    /// <param name="formatTime">Formats the time of a draft.</param>
    public RecoverDraftsViewModel(
        IDraftStore store,
        DraftScope scope,
        Func<DraftKey, Task> open,
        Func<string, string>? docTypeLabel = null,
        Func<DateTimeOffset, string>? formatTime = null)
    {
        _store = store ?? throw new ArgumentNullException(nameof(store));
        _scope = scope?.Validated() ?? throw new ArgumentNullException(nameof(scope));
        _open = open ?? throw new ArgumentNullException(nameof(open));
        _docTypeLabel = docTypeLabel ?? (name => name);
        _formatTime = formatTime ?? (t => t.ToLocalTime().ToString("g", CultureInfo.CurrentCulture));
        OpenCommand = new AsyncRelayCommand(p => p is RecoverDraftItem item ? _open(item.Key) : Task.CompletedTask);
        DiscardCommand = new AsyncRelayCommand(p => p is RecoverDraftItem item ? DiscardAsync(item) : Task.CompletedTask);
        DiscardAllCommand = new AsyncRelayCommand(_ => DiscardAllAsync(), _ => Items.Count > 0);
    }

    /// <summary>The drafts, newest first.</summary>
    public ObservableCollection<RecoverDraftItem> Items { get; } = [];

    /// <summary>The list heading.</summary>
    public string Title => "Recover drafts";

    /// <summary>The line shown when there is nothing to recover.</summary>
    public string EmptyText => "No unsaved drafts.";

    /// <summary>True when there is at least one draft.</summary>
    public bool HasDrafts => Items.Count > 0;

    /// <summary>Opens a draft's document (parameter: the item).</summary>
    public AsyncRelayCommand OpenCommand { get; }

    /// <summary>Deletes one draft (parameter: the item).</summary>
    public AsyncRelayCommand DiscardCommand { get; }

    /// <summary>Deletes every draft in the list.</summary>
    public AsyncRelayCommand DiscardAllCommand { get; }

    /// <summary>Reads the store again.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        var drafts = await _store.ListAsync(_scope, cancellationToken).ConfigureAwait(true);
        Items.Clear();
        foreach (var draft in drafts)
        {
            Items.Add(new RecoverDraftItem(draft, Describe(draft.Key, draft.Title), _formatTime(draft.SavedAtUtc)));
        }

        OnPropertyChanged(nameof(HasDrafts));
        DiscardAllCommand.RaiseCanExecuteChanged();
    }

    private string Describe(DraftKey key, string? title)
    {
        var label = _docTypeLabel(key.DocType);
        var what = key.IsNewDocument ? "New " + label : label + " " + key.DocumentId;
        return string.IsNullOrWhiteSpace(title) ? what : what + " - " + title;
    }

    private async Task DiscardAsync(RecoverDraftItem item)
    {
        await _store.DeleteAsync(item.Key).ConfigureAwait(true);
        _ = Items.Remove(item);
        OnPropertyChanged(nameof(HasDrafts));
        DiscardAllCommand.RaiseCanExecuteChanged();
    }

    private async Task DiscardAllAsync()
    {
        foreach (var item in Items.ToList())
        {
            await _store.DeleteAsync(item.Key).ConfigureAwait(true);
        }

        Items.Clear();
        OnPropertyChanged(nameof(HasDrafts));
        DiscardAllCommand.RaiseCanExecuteChanged();
    }
}
