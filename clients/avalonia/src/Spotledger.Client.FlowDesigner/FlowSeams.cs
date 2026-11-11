using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner;

/// How serious a problem is. A hint is the client's own UX advice, never an error.
public enum FlowProblemSeverity
{
    Error,
    Warning,
    Hint,
}

/// One problem: the server's, or a local hint. The code is the contract; the message is what a
/// person reads. A problem may point at a node, at an edge, or at neither (the process as a whole).
public sealed record FlowProblem(
    string Code,
    string Message,
    FlowProblemSeverity Severity,
    string? NodeId = null,
    string? EdgeFrom = null,
    string? EdgeTo = null)
{

    public bool IsHint => Severity == FlowProblemSeverity.Hint;

    public FlowEdgeKey? Edge =>
        EdgeFrom is null || EdgeTo is null ? null : new FlowEdgeKey(EdgeFrom, EdgeTo);
}

/// The server's answer to a validation request.
public sealed record FlowValidationReport(IReadOnlyList<FlowProblem> Problems, int Revision);

/// One row of the (read-only) version list.
public sealed record FlowVersionSummary(
    int Number,
    FlowVersionStatus Status,
    string? Author = null,
    DateTimeOffset? SavedAt = null,
    string? ContentHash = null);

/// The server's answer to a save.
public sealed record FlowDraftSaved(int Version, string ContentHash);

/// The server's answer to a publish request: it was accepted for review, not published.
public sealed record FlowPublishAccepted(string? ReviewId);

/// What a person may do with this process. The server decides; the client only obeys.
public sealed record FlowPermissions(bool CanRead, bool CanEdit, bool CanPublish)
{

    public static FlowPermissions Editor { get; } = new(true, true, true);

    public static FlowPermissions Reader { get; } = new(true, false, false);
}

/// Everything the designer asks the server: the kinds it offers, validation of the text form, the
/// save, the publish request and the version list. The server is authoritative for all of them;
/// a host binds this to the typed client, and the tests bind it to a fake. The seam keeps Api and
/// Metadata knowledge out of this project, exactly like the graph project's edit seam.
public interface IFlowDesignerServer
{
    Task<FlowKindCatalogue> LoadKindsAsync(CancellationToken cancellationToken);
    Task<FlowValidationReport> ValidateAsync(string textForm, CancellationToken cancellationToken);
    Task<FlowDraftSaved> SaveDraftAsync(string textForm, string? expectedContentHash, CancellationToken cancellationToken);
    Task<FlowPublishAccepted> RequestPublishAsync(string textForm, CancellationToken cancellationToken);
    Task<IReadOnlyList<FlowVersionSummary>> ListVersionsAsync(CancellationToken cancellationToken);
}
