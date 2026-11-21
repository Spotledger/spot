namespace Spotledger.Client.Shell;

/// <summary>
/// One open tab. Identity is a stable key: a document UUID for a record tab (ADR-33 —
/// never the document number) or a <c>workspace:&lt;name&gt;</c> key for a workspace page,
/// so re-opening an open record focuses its tab instead of duplicating it.
/// </summary>
public sealed class WorkspaceTabViewModel : ObservableObject
{
    private string _title;
    private bool _isPinned;
    private bool _isDirty;

    public WorkspaceTabViewModel(string workspace, string? title = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(workspace);
        Workspace = workspace;
        Key = "workspace:" + workspace;
        _title = title ?? workspace;
    }

    /// <summary>A record tab keyed by the record UUID; the document number is display only.</summary>
    public WorkspaceTabViewModel(string workspace, Guid documentId, string title, string? documentNumber = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(workspace);
        Workspace = workspace;
        DocumentId = documentId;
        Key = "document:" + documentId.ToString("N");
        _title = title;
        DocumentNumber = documentNumber;
    }

    /// <summary>A tab built by a registered tab kind (a document form, a list): the key comes from
    /// the request, the content is whatever the kind built.</summary>
    public WorkspaceTabViewModel(string workspace, string key, string title, object content)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(workspace);
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        Workspace = workspace;
        Key = key;
        _title = title;
        Content = content ?? throw new ArgumentNullException(nameof(content));
    }

    /// <summary>What a tab kind built for this tab (a view model); null for a workspace page.</summary>
    public object? Content { get; }

    /// <summary>Asked before the tab closes while it is dirty, in place of the shell's own prompt:
    /// true closes, false keeps the tab. Set by the host for content that owns its own guard.</summary>
    public Func<Task<bool>>? CloseGuard { get; set; }

    /// <summary>The navigation workspace this tab belongs to (Home, Selling, ...).</summary>
    public string Workspace { get; }

    /// <summary>Stable tab identity: <c>document:&lt;uuid&gt;</c> or <c>workspace:&lt;name&gt;</c>.</summary>
    public string Key { get; }

    /// <summary>The record UUID for document tabs; null for workspace pages.</summary>
    public Guid? DocumentId { get; }

    /// <summary>Display number of a posted document; a new draft has none yet.</summary>
    public string? DocumentNumber { get; }

    /// <summary>One line saying what the workspace holds (unbuilt pages show this).</summary>
    public string? Description { get; init; }

    /// <summary>The tab header text.</summary>
    public string Title
    {
        get => _title;
        set => SetField(ref _title, value);
    }

    /// <summary>Pinned tabs survive Close Others / Close All.</summary>
    public bool IsPinned
    {
        get => _isPinned;
        set => SetField(ref _isPinned, value);
    }

    /// <summary>Unsaved changes: closing asks Save, Discard or Cancel.</summary>
    public bool IsDirty
    {
        get => _isDirty;
        set
        {
            if (SetField(ref _isDirty, value))
            {
                OnPropertyChanged(nameof(ShowsUnsavedDot));
            }
        }
    }

    /// <summary>True when the tab header shows the unsaved dot (dirty and not pinned-clean).</summary>
    public bool ShowsUnsavedDot => IsDirty;
}

/// <summary>The three answers the dirty-close dialog offers.</summary>
public enum TabCloseChoice
{
    Save,
    Discard,
    Cancel,
}
