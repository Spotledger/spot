using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>The shell side of an action: what a click asks for, and what came back.</summary>
public enum ActionEffectKind
{
    /// <summary>A declared server method call.</summary>
    Call,
    /// <summary>Navigate to a registered destination.</summary>
    Open,
    /// <summary>Navigate to a path under a declared route.</summary>
    Navigate
}

/// <summary>
/// One dispatch: exactly what the user's click asks the server for. The display is never
/// authorisation — the dispatcher sends it, and the server checks the user's rights again.
/// </summary>
/// <param name="Id">The action's id.</param>
/// <param name="App">The app that contributed the action.</param>
/// <param name="Kind">The effect kind.</param>
/// <param name="Doctype">The DocType of a call.</param>
/// <param name="Method">The declared method of a call.</param>
/// <param name="Arguments">The evaluated arguments, in document order.</param>
/// <param name="Destination">The destination of an <c>open</c>.</param>
/// <param name="Filters">The evaluated filters of an <c>open</c>.</param>
/// <param name="Path">The path of a <c>navigate_url</c>.</param>
/// <param name="DocumentId">The document the click was made on.</param>
/// <param name="DocumentVersion">The document's version, for the call's <c>If-Match</c>.</param>
/// <param name="IdempotencyKey">One key per click: a retry after a lost answer re-sends it and gets the stored answer.</param>
public sealed record ActionRequest(
    string Id,
    string App,
    ActionEffectKind Kind,
    string? Doctype = null,
    string? Method = null,
    IReadOnlyList<KeyValuePair<string, ExpressionValue>>? Arguments = null,
    string? Destination = null,
    IReadOnlyList<KeyValuePair<string, ExpressionValue>>? Filters = null,
    string? Path = null,
    string? DocumentId = null,
    int? DocumentVersion = null,
    string? IdempotencyKey = null);

/// <summary>What the dispatcher's answer asks the shell to do.</summary>
public enum ActionOutcomeKind
{
    /// <summary>Reload the document and the panels.</summary>
    Refresh,
    /// <summary>Navigate to a destination.</summary>
    Open,
    /// <summary>Show a message to the user.</summary>
    ShowMessage,
    /// <summary>Start a download.</summary>
    Download,
    /// <summary>The call failed; show the problem text once.</summary>
    Failed
}

/// <summary>The dispatcher's answer: one effect, already decoded from the server's reply.</summary>
/// <param name="Kind">What to do.</param>
/// <param name="Message">The message of a <see cref="ActionOutcomeKind.ShowMessage"/> or <see cref="ActionOutcomeKind.Failed"/>.</param>
/// <param name="Problem">The server's problem text for a failure.</param>
/// <param name="Destination">The destination of an <see cref="ActionOutcomeKind.Open"/>.</param>
/// <param name="Filters">The evaluated filters of an <see cref="ActionOutcomeKind.Open"/>.</param>
/// <param name="Reference">The download reference of an <see cref="ActionOutcomeKind.Download"/>.</param>
public sealed record ActionOutcome(
    ActionOutcomeKind Kind,
    string? Message = null,
    string? Problem = null,
    string? Destination = null,
    IReadOnlyList<KeyValuePair<string, ExpressionValue>>? Filters = null,
    string? Reference = null)
{
    /// <summary>A successful refresh.</summary>
    public static ActionOutcome Refresh { get; } = new(ActionOutcomeKind.Refresh);

    /// <summary>A failure with the server's problem text.</summary>
    public static ActionOutcome Failure(string problem) => new(ActionOutcomeKind.Failed, Problem: problem);
}

/// <summary>
/// The seam between the model and the network: the real implementation issues the native
/// API call (with the idempotency key and the document version) through
/// <c>Spotledger.Client.Api</c>; a test passes a fake and asserts what was asked for.
/// </summary>
public interface IActionDispatcher
{
    /// <summary>Dispatches one action request. Implementations must not block the UI thread.</summary>
    Task<ActionOutcome> DispatchAsync(ActionRequest request, CancellationToken cancellationToken);
}

/// <summary>
/// The shell effects an accepted answer is applied through: the confirmation before a
/// destructive send, a message, a document refresh, a navigation and a download. The model
/// never navigates, downloads or paints by itself.
/// </summary>
public interface IShellServices
{
    /// <summary>Asks the user to confirm <paramref name="text" />; false cancels the action.</summary>
    Task<bool> ConfirmAsync(string text, CancellationToken cancellationToken);

    /// <summary>Shows one message (an answer, or a failure's problem text).</summary>
    Task ShowMessageAsync(string message, CancellationToken cancellationToken);

    /// <summary>Reloads the document and the panels after a successful call.</summary>
    Task RefreshAsync(string? doctype, string? documentId, CancellationToken cancellationToken);

    /// <summary>Navigates to a registered destination.</summary>
    Task OpenAsync(string destination, IReadOnlyList<KeyValuePair<string, ExpressionValue>> filters, CancellationToken cancellationToken);

    /// <summary>Opens one document by id (a table row's link); the shell resolves its DocType.</summary>
    Task OpenDocumentAsync(string documentId, CancellationToken cancellationToken);

    /// <summary>Starts a download of the reference the answer named.</summary>
    Task DownloadAsync(string reference, CancellationToken cancellationToken);
}

/// <summary>
/// The shell's slot registry: the seam through which this project's renderer plugs into
/// the shell without editing the shell project (which WP-UI-13.T3 does not own). The shell
/// asks for a slot's host when it builds that slot, and shows a notice when a document was
/// refused whole.
/// </summary>
public interface IShellSlotRegistry
{
    /// <summary>Registers the factory that builds one slot's host (`doc.toolbar`, `doc.sidepanel`, `home.tiles`).</summary>
    /// <param name="slot">The slot's full name.</param>
    /// <param name="hostFactory">Builds the host control (an opaque object to this project) for a DocType, null for a slot that is not DocType-scoped.</param>
    void RegisterSlotHost(string slot, Func<string?, object?> hostFactory);

    /// <summary>Shows the "update the app" notice, with the code and message of the refusal.</summary>
    void ShowNotice(string code, string message);
}
