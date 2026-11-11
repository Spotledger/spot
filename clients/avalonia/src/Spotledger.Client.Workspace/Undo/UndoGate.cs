using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Workspace;

/// <summary>
/// The document-state rule the undo history and a draft restore share. A posted, cancelled or
/// unrecognised document takes no change, decided exactly as <see cref="ChildTableModel.TableRefusal"/>
/// decides it: a state the server names but this client does not know fails closed, and a document that
/// carries no state at all is an ordinary editable draft.
///
/// <para>One predicate, so the recovery bar cannot restore into a document the stack would refuse to
/// undo: a host passes <see cref="IsDocumentReadOnly"/> as the recovery bar's
/// <c>isDocumentReadOnly</c>, and the stack asks <see cref="DocumentRefusal"/> for every step.</para>
/// </summary>
public static class UndoGate
{
    /// <summary>Why the document refuses every change now, or null when it takes edits.</summary>
    public static ChildTableResult? DocumentRefusal(DocumentDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        return draft.Status switch
        {
            DocumentStatus.Posted => ChildTableResult.Refused(ChildTableRefusal.DocumentPosted),
            DocumentStatus.Cancelled => ChildTableResult.Refused(ChildTableRefusal.DocumentCancelled),
            // The server named a state this client does not know: fail closed.
            DocumentStatus.Unknown when draft.RawStatus is not null => ChildTableResult.Refused(ChildTableRefusal.DocumentStateUnknown),
            _ => null,
        };
    }

    /// <summary>True when the document is read-only now (what a host passes as <c>isDocumentReadOnly</c>).</summary>
    public static bool IsDocumentReadOnly(DocumentDraft draft) => DocumentRefusal(draft) is not null;
}
