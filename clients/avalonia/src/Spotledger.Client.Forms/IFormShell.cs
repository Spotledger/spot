using Spotledger.Client.Metadata;

namespace Spotledger.Client.Forms;

/// <summary>How a person resolved a version conflict.</summary>
public enum ConflictChoice
{
    /// <summary>Leave the form as it is; nothing is sent.</summary>
    Keep,

    /// <summary>Load the server's version and re-apply my changes on top of it.</summary>
    ReloadAndReapply,

    /// <summary>Send my changes against the server's current version, replacing what changed there.</summary>
    Overwrite,
}

/// <summary>What the conflict panel shows.</summary>
/// <param name="DocumentId">The document.</param>
/// <param name="YourVersion">The version my edits were based on.</param>
/// <param name="CurrentVersion">The server's version, when it said.</param>
/// <param name="Changed">The paths I changed.</param>
public sealed record ConflictInfo(string? DocumentId, int YourVersion, int? CurrentVersion, IReadOnlyList<string> Changed);

/// <summary>A document that still refers to the one being deleted or cancelled.</summary>
/// <param name="Reference">What refers to it (the server's text).</param>
/// <param name="Detail">More about the reference, when the server gave it.</param>
public sealed record Referrer(string Reference, string? Detail);

/// <summary>
/// The dialogs and tab operations a form needs from its host. A form never opens a window itself:
/// the shell implements this over its dialogs, so the form is testable without a window.
/// </summary>
public interface IFormShell
{
    /// <summary>Asks a yes/no question (submit, delete). False declines.</summary>
    Task<bool> ConfirmAsync(string title, string message, string confirmLabel);

    /// <summary>Asks for a line of text (a cancel reason, a new name). Null cancels.</summary>
    Task<string?> AskTextAsync(string title, string prompt, bool required);

    /// <summary>Lists what changed and asks whether to discard it. False keeps editing.</summary>
    Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed);

    /// <summary>Shows the conflict panel and returns the person's choice.</summary>
    Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info);

    /// <summary>Shows the documents that block a delete or a cancel.</summary>
    Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers);

    /// <summary>Opens a document (<paramref name="id"/> null opens a new one) in its own tab.</summary>
    void OpenDocument(string doctype, string? id);

    /// <summary>Closes this form's tab without asking (the form already did).</summary>
    void CloseTab();
}

/// <summary>A shell for a form with no host: every question is declined and nothing opens.</summary>
public sealed class NullFormShell : IFormShell
{
    /// <inheritdoc />
    public Task<bool> ConfirmAsync(string title, string message, string confirmLabel) => Task.FromResult(false);

    /// <inheritdoc />
    public Task<string?> AskTextAsync(string title, string prompt, bool required) => Task.FromResult<string?>(null);

    /// <inheritdoc />
    public Task<bool> ConfirmDiscardAsync(IReadOnlyList<string> changed) => Task.FromResult(false);

    /// <inheritdoc />
    public Task<ConflictChoice> ChooseConflictAsync(ConflictInfo info) => Task.FromResult(ConflictChoice.Keep);

    /// <inheritdoc />
    public Task ShowReferrersAsync(string title, IReadOnlyList<Referrer> referrers) => Task.CompletedTask;

    /// <inheritdoc />
    public void OpenDocument(string doctype, string? id)
    {
    }

    /// <inheritdoc />
    public void CloseTab()
    {
    }
}

/// <summary>
/// The seam draft autosave plugs into (WP-UI-17). The form calls it after every change to the
/// draft; nothing is stored in this task, and the default is no autosave.
/// </summary>
public interface IDraftAutosave
{
    /// <summary>The draft of <paramref name="doctype"/> (document <paramref name="documentId"/>, null while new) changed.</summary>
    void DraftChanged(string doctype, string? documentId, DocumentDraft draft);

    /// <summary>The draft was saved or discarded: any stored copy can go.</summary>
    void DraftSettled(string doctype, string? documentId);
}
