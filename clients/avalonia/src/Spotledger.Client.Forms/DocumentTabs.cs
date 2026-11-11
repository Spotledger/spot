using Spotledger.Client.Shell;

namespace Spotledger.Client.Forms;

/// <summary>
/// The document form as a workspace tab kind: the key scheme (one tab per DocType and id), the
/// registration in the shell's tab-kind registry, and the commands the launcher shows.
/// </summary>
public static class DocumentTabs
{
    /// <summary>The tab kind name.</summary>
    public const string Kind = WorkspaceTabKinds.Document;

    /// <summary>The unique key of the tab for a document; a new document has its own key per request.</summary>
    public static string KeyFor(string doctype, string? id) => WorkspaceTabKinds.DocumentKey(doctype, id);

    /// <summary>Registers the form as the <c>document</c> tab kind: <paramref name="create"/> builds the
    /// form's view model for a request.</summary>
    public static void Register(WorkspaceTabKinds kinds, Func<string, string?, DocumentFormViewModel> create)
    {
        ArgumentNullException.ThrowIfNull(kinds);
        ArgumentNullException.ThrowIfNull(create);
        kinds.Register(Kind, request =>
        {
            var (doctype, id) = WorkspaceTabKinds.ParseDocumentRequest(request);
            return create(doctype, id);
        });
    }

    /// <summary>Keeps a workspace tab in step with its form: the title, the unsaved marker, and
    /// (through <see cref="DocumentFormViewModel.RequestCloseAsync"/>) the close guard. Returns the
    /// subscription's disposer.</summary>
    public static IDisposable Bind(WorkspaceTabViewModel tab, DocumentFormViewModel form)
    {
        ArgumentNullException.ThrowIfNull(tab);
        ArgumentNullException.ThrowIfNull(form);
        void Sync()
        {
            tab.Title = form.Title;
            tab.IsDirty = form.IsDirty;
        }

        void Changed(object? sender, System.ComponentModel.PropertyChangedEventArgs e)
        {
            if (e.PropertyName is nameof(DocumentFormViewModel.Title) or nameof(DocumentFormViewModel.IsDirty))
            {
                Sync();
            }
        }

        form.PropertyChanged += Changed;
        Sync();
        return new Unsubscribe(() => form.PropertyChanged -= Changed);
    }

    private sealed class Unsubscribe(Action action) : IDisposable
    {
        public void Dispose() => action();
    }

    /// <summary>The request that opens document <paramref name="id"/> of <paramref name="doctype"/>.</summary>
    public static WorkspaceTabRequest Open(string doctype, string id, string? title = null)
        => WorkspaceTabKinds.DocumentRequest(doctype, id, title);

    /// <summary>The request that opens a new document of <paramref name="doctype"/>.</summary>
    public static WorkspaceTabRequest New(string doctype, string? title = null)
        => WorkspaceTabKinds.DocumentRequest(doctype, null, title);
}
