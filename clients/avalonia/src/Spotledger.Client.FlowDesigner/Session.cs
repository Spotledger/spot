using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Spotledger.Client.FlowDesigner;

/// The outcome of one designer action: what to tell the person, or why it was refused.
public sealed record FlowActionResult(bool Ok, string? Message = null, string? Error = null)
{

    public static FlowActionResult Success(string message) => new(true, message);

    public static FlowActionResult Refused(string error) => new(false, Error: error);
}

/// One designer session: the document, its editor, the server catalogue and the three actions the
/// brief asks for — save as draft, ask for publication, and list versions. Permissions come from
/// the server; a reader gets a read-only document (no editing affordance active, no save sent) and
/// an editor without the publish right is told why rather than being shown a dead button.
///
/// No method reads the wall clock: the host passes nothing time-based here, and the timestamps in
/// the version list are the server's own.
public sealed class FlowDesignerSession
{

    private readonly IFlowDesignerServer _server;

    private string? _baselineHash;

    public FlowDesignerSession(
        IFlowDesignerServer server,
        FlowDocument document,
        FlowPermissions permissions,
        FlowStrings? strings = null)
    {
        ArgumentNullException.ThrowIfNull(server);
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(permissions);
        _server = server;
        Document = document;
        Permissions = permissions;
        Strings = strings ?? new FlowStrings();
        Editor = new FlowEditor(document) { CanEdit = permissions.CanEdit };
        Editor.Changed += (_, _) => Changed?.Invoke(this, EventArgs.Empty);
    }

    public FlowDocument Document { get; }

    public FlowEditor Editor { get; }

    public FlowPermissions Permissions { get; }

    public FlowStrings Strings { get; }

    public FlowKindCatalogue Catalogue { get; private set; } = FlowKindCatalogue.Empty;

    public IReadOnlyList<FlowVersionSummary> Versions { get; private set; } = [];

    public string? StatusMessage { get; private set; }

    public string? ErrorMessage { get; private set; }

    public bool IsReadOnly => !Permissions.CanEdit;

    public bool IsBusy { get; private set; }

    public event EventHandler? Changed;

    public async Task<FlowKindCatalogue> LoadKindsAsync(CancellationToken cancellationToken)
    {
        Catalogue = await _server.LoadKindsAsync(cancellationToken).ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return Catalogue;
    }

    public async Task<FlowActionResult> SaveDraftAsync(CancellationToken cancellationToken)
    {
        if (!Permissions.CanEdit)
        {
            return Refuse(FlowStrings.RefusedReadOnly);
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            FlowDraftSaved saved = await _server
                .SaveDraftAsync(Document.ToText(), _baselineHash, cancellationToken)
                .ConfigureAwait(true);
            _baselineHash = saved.ContentHash;
            Document.Version = saved.Version;
            StatusMessage = FlowStrings.DraftSaved(saved.Version);
            ErrorMessage = null;
            Editor.ClearHistory();
            return FlowActionResult.Success(StatusMessage);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    public async Task<FlowActionResult> RequestPublishAsync(CancellationToken cancellationToken)
    {
        if (!Permissions.CanEdit)
        {
            return Refuse(FlowStrings.RefusedReadOnly);
        }

        if (!Permissions.CanPublish)
        {
            return Refuse(FlowStrings.PublishDenied);
        }

        IsBusy = true;
        Changed?.Invoke(this, EventArgs.Empty);
        try
        {
            FlowPublishAccepted accepted = await _server
                .RequestPublishAsync(Document.ToText(), cancellationToken)
                .ConfigureAwait(true);
            StatusMessage = FlowStrings.PublishRequested;
            ErrorMessage = null;
            _ = accepted.ReviewId;
            return FlowActionResult.Success(StatusMessage);
        }
        finally
        {
            IsBusy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    public async Task<IReadOnlyList<FlowVersionSummary>> LoadVersionsAsync(CancellationToken cancellationToken)
    {
        Versions = await _server.ListVersionsAsync(cancellationToken).ConfigureAwait(true);
        Changed?.Invoke(this, EventArgs.Empty);
        return Versions;
    }

    public void ReportError(string message)
    {
        ErrorMessage = message;
        StatusMessage = null;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    private FlowActionResult Refuse(string reason)
    {
        ErrorMessage = reason;
        StatusMessage = null;
        Changed?.Invoke(this, EventArgs.Empty);
        return FlowActionResult.Refused(reason);
    }
}
