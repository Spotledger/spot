namespace Spotledger.Client.Extensions;

/// <summary>
/// One problem the client found in a resolved extension document, or one that happened
/// while a model was evaluated. <see cref="Code"/> is a stable code: the <c>uiext.*</c>
/// vocabulary of <c>contracts/ui-ext/errors.md</c> for a document rule, an <c>expr.*</c>
/// code carried in the message for a failed evaluation, and <c>uiext.client.*</c> for the
/// three refusals only a client can make (a newer contract, an oversized document, a
/// runtime evaluation failure).
/// </summary>
/// <param name="Id">The item id the problem belongs to, or the document's pointer when it is not item-scoped.</param>
/// <param name="Code">The stable code.</param>
/// <param name="Message">The human message, with the app's vocabulary where the contract has one.</param>
public sealed record ExtensionDiagnostic(string Id, string Code, string Message)
{
    /// <inheritdoc/>
    public override string ToString() => $"{Code}: {Id}: {Message}";
}

/// <summary>The stable diagnostic codes the client reports. The document codes mirror
/// <c>contracts/ui-ext/errors.md</c> one for one; a client-only code is marked as such.</summary>
public static class ExtensionCodes
{
    /// <summary>A document does not fit the schema in a way no other code names (closed object, wrong type, unknown control).</summary>
    public const string Schema = "uiext.schema";
    /// <summary><c>slot</c> is not in the closed slot list.</summary>
    public const string UnknownSlot = "uiext.unknown_slot";
    /// <summary>The item kind is not what the slot accepts.</summary>
    public const string SlotKindMismatch = "uiext.slot_kind_mismatch";
    /// <summary>An item in a <c>doc.*</c> or <c>list.*</c> slot names no DocType.</summary>
    public const string DoctypeRequired = "uiext.doctype_required";
    /// <summary>More items than the slot allows per app and DocType.</summary>
    public const string SlotLimit = "uiext.slot_limit";
    /// <summary>A literal label is longer than the slot allows.</summary>
    public const string LabelTooLong = "uiext.label_too_long";
    /// <summary>An icon is not a token of the closed icon set.</summary>
    public const string UnknownIcon = "uiext.unknown_icon";
    /// <summary>An action has none, or more than one, of <c>call</c>, <c>open</c>, <c>navigate_url</c>.</summary>
    public const string EffectInvalid = "uiext.effect_invalid";
    /// <summary><c>navigate_url</c> leaves the app (a scheme or host, or not an absolute path).</summary>
    public const string ExternalUrl = "uiext.external_url";
    /// <summary>A path is not under a declared route.</summary>
    public const string UnknownRoute = "uiext.unknown_route";
    /// <summary>A call names a method the manifest does not declare for that DocType.</summary>
    public const string UnknownMethod = "uiext.unknown_method";
    /// <summary>A destination is not in the registry the manifest declares.</summary>
    public const string UnknownDestination = "uiext.unknown_destination";
    /// <summary>A permission reference does not exist.</summary>
    public const string UnknownPermission = "uiext.unknown_permission";
    /// <summary>An expression does not parse in the shared language.</summary>
    public const string ExpressionInvalid = "uiext.expression_invalid";
    /// <summary>A text block contains markup, an image or a link that is not <c>dest:</c>.</summary>
    public const string RawHtml = "uiext.raw_html";
    /// <summary>A table or stat block names no query.</summary>
    public const string QueryMissing = "uiext.query_missing";
    /// <summary>A block or tile prop names a query the manifest does not declare.</summary>
    public const string UnknownQuery = "uiext.unknown_query";
    /// <summary>A table names an aggregate query, or a stat a rows query.</summary>
    public const string QueryKind = "uiext.query_kind";
    /// <summary>A tab names a panel the document does not contribute.</summary>
    public const string UnknownPanel = "uiext.unknown_panel";
    /// <summary>A tile or <c>use</c> node names a component the document does not contribute.</summary>
    public const string UnknownComponent = "uiext.unknown_component";
    /// <summary>A control's <c>on_click</c> names an action the document does not contribute.</summary>
    public const string UnknownAction = "uiext.unknown_action";
    /// <summary>An item's origin or id namespace is not the contributing app.</summary>
    public const string OriginMismatch = "uiext.origin_mismatch";
    /// <summary>Two items share an id.</summary>
    public const string DuplicateId = "uiext.duplicate_id";
    /// <summary>Components reference each other in a cycle.</summary>
    public const string ComponentCycle = "uiext.component_cycle";
    /// <summary>An expansion nests deeper than the contract allows.</summary>
    public const string ComponentTooDeep = "uiext.component_too_deep";
    /// <summary>An expansion has more nodes than the contract allows.</summary>
    public const string ComponentTooLarge = "uiext.component_too_large";

    /// <summary>Client only: the document's <c>contract_version</c> major is newer than this client implements.</summary>
    public const string UnsupportedContract = "uiext.client.unsupported_contract";
    /// <summary>Client only: the document is larger than the client's parse limit.</summary>
    public const string DocumentTooLarge = "uiext.client.document_too_large";
    /// <summary>Client only: an expression failed while it was evaluated (the <c>expr.*</c> code is in the message).</summary>
    public const string ExpressionFailed = "uiext.client.expression_failed";
    /// <summary>Client only: dispatching an action failed; the server's problem text is in the message.</summary>
    public const string DispatchFailed = "uiext.client.dispatch_failed";
    /// <summary>Client only: a declared query failed; the server's problem text is in the message.</summary>
    public const string QueryFailed = "uiext.client.query_failed";
}

/// <summary>
/// The diagnostics of one document and of the models built from it. Reports once per
/// (id, code, message) so a document problem is not repeated for every evaluation, and so
/// a failing expression reports once however often its slot is refreshed. Thread-safe: a
/// query result or an evaluation may report from a continuation.
/// </summary>
public sealed class DiagnosticSink
{
    private readonly List<ExtensionDiagnostic> _items = [];
    private readonly HashSet<string> _seen = new(StringComparer.Ordinal);
    private readonly HashSet<string> _once = new(StringComparer.Ordinal);
    private readonly Lock _gate = new();

    /// <summary>Raised for every diagnostic that was actually added (never for a duplicate).</summary>
    public event EventHandler<ExtensionDiagnostic>? Reported;

    /// <summary>A snapshot of everything reported so far, in report order.</summary>
    public IReadOnlyList<ExtensionDiagnostic> Items
    {
        get
        {
            lock (_gate)
            {
                return [.. _items];
            }
        }
    }

    /// <summary>Adds a diagnostic unless the same (id, code, message) was already added.</summary>
    /// <returns>True when it was added.</returns>
    public bool Report(string id, string code, string message)
    {
        ArgumentNullException.ThrowIfNull(id);
        ArgumentNullException.ThrowIfNull(code);
        ArgumentNullException.ThrowIfNull(message);
        return Add(new ExtensionDiagnostic(id, code, message), $"{id}\u001f{code}\u001f{message}", _seen);
    }

    /// <summary>Adds at most one diagnostic per (id, code), however often it is reported. This
    /// is the expression rule: an expression that fails evaluates to hidden and logs once.</summary>
    /// <returns>True when it was added.</returns>
    public bool ReportOnce(string id, string code, string message)
    {
        ArgumentNullException.ThrowIfNull(id);
        ArgumentNullException.ThrowIfNull(code);
        ArgumentNullException.ThrowIfNull(message);
        return Add(new ExtensionDiagnostic(id, code, message), $"{id}\u001f{code}", _once);
    }

    /// <summary>Adds a diagnostic produced elsewhere.</summary>
    public bool Report(ExtensionDiagnostic diagnostic)
    {
        ArgumentNullException.ThrowIfNull(diagnostic);
        return Report(diagnostic.Id, diagnostic.Code, diagnostic.Message);
    }

    private bool Add(ExtensionDiagnostic diagnostic, string key, HashSet<string> keys)
    {
        EventHandler<ExtensionDiagnostic>? handler;
        lock (_gate)
        {
            if (!keys.Add(key))
            {
                return false;
            }
            _items.Add(diagnostic);
            handler = Reported;
        }
        handler?.Invoke(this, diagnostic);
        return true;
    }
}
