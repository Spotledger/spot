namespace Spotledger.Client.Designer;

/// <summary>What the designer needs from the server's draft model, and nothing more.</summary>
/// <remarks>
/// Every member is a call the server makes the decision in: it applies the op (or refuses it with
/// a friendly issue), it validates, it previews, it plans the publish and it checks the
/// <c>designer.publish</c> permission. The designer never evaluates a condition or validates a
/// design itself, so a client bug cannot make a bad design look good.
///
/// The HTTP adapter for this surface (over the generated native API client) belongs with the
/// generated client; the tests drive the contract through their own in-memory server.
/// </remarks>
public interface IDesignerServer
{
    /// <summary>Opens a draft and returns its current state.</summary>
    Task<DraftDocument> OpenAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>
    /// Applies one op to the draft at <paramref name="baseVersion"/>: the changed draft, or the
    /// refusal as a friendly issue. An op that does not change the draft still returns it.
    /// </summary>
    Task<OpOutcome> ApplyAsync(string draftId, DesignerOp op, long baseVersion, CancellationToken cancellationToken = default);

    /// <summary>Moves the server's op-log cursor back one op.</summary>
    Task<DraftDocument> UndoAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Moves the server's op-log cursor forward one op.</summary>
    Task<DraftDocument> RedoAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Autosaves the draft at <paramref name="version"/> (the designer's autosave).</summary>
    Task SaveAsync(string draftId, long version, CancellationToken cancellationToken = default);

    /// <summary>Validates the draft at a version and returns the server's plain-language report.</summary>
    Task<ValidationReportSnapshot> ValidateAsync(string draftId, long version, CancellationToken cancellationToken = default);

    /// <summary>
    /// Compiles the visual condition to expression text on the server, for the builder's preview.
    /// A refusal comes back as the second element (a message in the user's words).
    /// </summary>
    Task<(string? Text, string? Error)> PreviewConditionAsync(string draftId, ConditionExpression condition, CancellationToken cancellationToken = default);

    /// <summary>Classifies a type change against the values already stored.</summary>
    Task<TypeChangePlanSummary> PlanTypeChangeAsync(string draftId, string field, string newType, string? childDoctype = null, CancellationToken cancellationToken = default);

    /// <summary>Builds the ephemeral preview: metadata overlay, layout and sample data. Nothing is persisted.</summary>
    Task<PreviewModel> PreviewAsync(string draftId, PreviewLayout layout, CancellationToken cancellationToken = default);

    /// <summary>Builds the publish plan summary (risk, changes, readiness).</summary>
    Task<PublishPlanSummary> PlanPublishAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Publishes a ready plan; the server checks the permission and the confirmation.</summary>
    Task<PublishReceipt> PublishAsync(string draftId, string planId, CancellationToken cancellationToken = default);

    /// <summary>Discards the draft; it can never be published afterwards.</summary>
    Task DiscardAsync(string draftId, CancellationToken cancellationToken = default);

    /// <summary>Searches the site's DocTypes for the link picker.</summary>
    Task<IReadOnlyList<DocTypeChoice>> SearchDocTypesAsync(string draftId, string query, CancellationToken cancellationToken = default);
}
