using Avalonia.Controls;
using Spotledger.Client.Forms;

namespace Spotledger.Client.Workspace.Split;

/// <summary>
/// The documents the arrangement has open: exactly one form view model and one view per document id,
/// created on first selection and reused afterwards. Opening the same document in a tab moves it out of
/// the arrangement instead of building a second form for it, which is what "tab dedup by document id
/// holds across split and tab" means in practice: the count of created forms never exceeds the count of
/// distinct documents a person looked at.
/// </summary>
public sealed class SplitDocumentHost : IDisposable
{
    private readonly Func<string?, DocumentFormViewModel> _create;
    private readonly Dictionary<string, Entry> _open = new(StringComparer.Ordinal);
    private readonly HashSet<string> _inTabs = new(StringComparer.Ordinal);
    private bool _disposed;

    /// <summary>Creates the host. <paramref name="createForm"/> receives the document id (null: a new document).</summary>
    public SplitDocumentHost(Func<string?, DocumentFormViewModel> createForm)
    {
        _create = createForm ?? throw new ArgumentNullException(nameof(createForm));
    }

    /// <summary>The form view model of <paramref name="id"/>, created on first use.</summary>
    /// <param name="id">The document id; null opens the new-document form.</param>
    public DocumentFormViewModel Model(string? id) => Get(id).Model;

    /// <summary>The view of <paramref name="id"/>, created on first use.</summary>
    public DocumentFormView View(string? id) => Get(id).View;

    /// <summary>How many forms this host has created (a dedup assertion reads it).</summary>
    public int Created { get; private set; }

    /// <summary>The keys that are open here, in creation order.</summary>
    public IReadOnlyList<string> OpenIds => [.. _open.Keys];

    /// <summary>True when the document was moved to its own tab and must not be shown here again.</summary>
    public bool IsInTab(string? id) => _inTabs.Contains(Key(id));

    /// <summary>Records that a document is open in a tab (the tab host tells the arrangement).</summary>
    public void MarkInTab(string? id) => _ = _inTabs.Add(Key(id));

    /// <summary>Forgets that a document is open in a tab (its tab was closed).</summary>
    public bool ReleaseTab(string? id) => _inTabs.Remove(Key(id));

    /// <summary>
    /// Moves <paramref name="id"/> to its own tab: the form is disposed here (the tab's own form takes
    /// over) and <paramref name="openInTab"/> is called once. False when there is nothing to move.
    /// </summary>
    public bool MoveToTab(string? id, Action<string?>? openInTab = null)
    {
        var key = Key(id);
        if (!_open.Remove(key, out var entry))
        {
            return false;
        }

        entry.View.Dispose();
        entry.Model.Dispose();
        _ = _inTabs.Add(key);
        openInTab?.Invoke(id);
        return true;
    }

    /// <summary>Moves the open form from <paramref name="from"/> to <paramref name="to"/> (a saved new document).</summary>
    public void Rekey(string? from, string? to)
    {
        var source = Key(from);
        var target = Key(to);
        if (string.Equals(source, target, StringComparison.Ordinal) || !_open.Remove(source, out var entry))
        {
            return;
        }

        _open[target] = entry;
    }

    /// <summary>
    /// Settles a document: its changes were saved (and a new document now has an id) or it was deleted,
    /// so the form is disposed and the id forgotten.
    /// </summary>
    public void Settle(string? id)
    {
        var key = Key(id);
        if (_open.Remove(key, out var entry))
        {
            entry.View.Dispose();
            entry.Model.Dispose();
        }

        _ = _inTabs.Remove(key);
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        foreach (var entry in _open.Values)
        {
            entry.View.Dispose();
            entry.Model.Dispose();
        }

        _open.Clear();
        _inTabs.Clear();
    }

    private Entry Get(string? id)
    {
        ObjectDisposedException.ThrowIf(_disposed, this);
        var key = Key(id);
        if (_open.TryGetValue(key, out var existing))
        {
            return existing;
        }

        if (_inTabs.Contains(key))
        {
            throw new InvalidOperationException(
                "The document is open in its own tab: the arrangement may not build a second form for it.");
        }

        var model = _create(id);
        var view = new DocumentFormView(model);
        Created++;
        var entry = new Entry(model, view);
        _open[key] = entry;
        return entry;
    }

    private static string Key(string? id) => id ?? string.Empty;

    private sealed record Entry(DocumentFormViewModel Model, DocumentFormView View);
}
