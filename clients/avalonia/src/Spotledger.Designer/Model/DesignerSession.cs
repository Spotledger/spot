namespace Spotledger.Client.Designer;

/// <summary>Where a new field lands on the form.</summary>
public sealed record DesignerPlacement(string SectionId, string? AfterField = null);

/// <summary>
/// The designer's command layer: every user action becomes one server op (or a short, ordered
/// run of them), the server's answer becomes the state the UI shows, and every accepted op is
/// autosaved and re-validated on the server.
/// </summary>
/// <remarks>
/// This class holds no design rules of its own. It builds op payloads, keeps the draft the server
/// returned, and exposes what the panels bind to. Consequences worth knowing:
/// <list type="bullet">
/// <item>undo and redo are the server's op-log cursor, not a client-side stack;</item>
/// <item>validation and the preview are server calls, so the client cannot mark a bad design good;</item>
/// <item>a command that the server refuses changes nothing and returns the refusal;</item>
/// <item>the commands the UI issued are kept in <see cref="Sent"/>, which is what the tests compare
/// with the server-side golden sequence.</item>
/// </list>
/// </remarks>
public sealed class DesignerSession
{
    private readonly IDesignerServer _server;
    private readonly List<DesignerOp> _sent = new();
    private DraftDocument? _draft;
    private PublishReceipt? _lastReceipt;

    /// <summary>Creates a session over one designer and one server surface.</summary>
    public DesignerSession(IDesignerServer server, string draftId)
    {
        _server = server ?? throw new ArgumentNullException(nameof(server));
        ArgumentException.ThrowIfNullOrEmpty(draftId);
        DraftId = draftId;
    }

    /// <summary>The draft this session edits.</summary>
    public string DraftId { get; }

    /// <summary>
    /// The server surface this session applies ops through. The designer shell hands it to the
    /// registered tools as part of <see cref="DesignerToolContext"/>, so a tool adapts the same
    /// connection the designer is using instead of opening its own.
    /// </summary>
    public IDesignerServer Server => _server;

    /// <summary>Raised after any state the panels show has changed.</summary>
    public event EventHandler? Changed;

    /// <summary>The draft as the server last reported it; null before <see cref="OpenAsync"/>.</summary>
    public DraftDocument? Draft => _draft;

    /// <summary>Whether a draft is open.</summary>
    public bool IsOpen => _draft is not null;

    /// <summary>The last validation report, or null before the first one.</summary>
    public ValidationReportSnapshot? Validation { get; private set; }

    /// <summary>What the last autosave did.</summary>
    public SaveState SaveState { get; private set; } = SaveState.Saved;

    /// <summary>The outcome of the last command.</summary>
    public DesignerResult LastResult { get; private set; } = DesignerResult.Success;

    /// <summary>The type change waiting for the designer to confirm it, if any.</summary>
    public TypeChangePlanSummary? PendingTypeChange { get; private set; }

    /// <summary>The publish plan waiting for confirmation, if any.</summary>
    public PublishPlanSummary? PendingPublish { get; private set; }

    /// <summary>The element the user selected, as a server element path.</summary>
    public string? SelectedElement { get; private set; }

    /// <summary>Every op this session sent, in order (the commands the UI issued).</summary>
    public IReadOnlyList<DesignerOp> Sent => _sent;

    /// <summary>Loads the draft.</summary>
    public async Task<DesignerResult> OpenAsync(CancellationToken cancellationToken = default)
    {
        _draft = await _server.OpenAsync(DraftId, cancellationToken).ConfigureAwait(false);
        return await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Re-reads the draft from the server (a refresh).</summary>
    public Task<DesignerResult> RefreshAsync(CancellationToken cancellationToken = default) =>
        IsOpen ? OpenAsync(cancellationToken) : throw new InvalidOperationException("No draft is open.");

    /// <summary>Saves now; autosave has already saved every accepted op, so this is a safety net.</summary>
    public async Task<DesignerResult> SaveAsync(CancellationToken cancellationToken = default)
    {
        await SaveNowAsync(cancellationToken).ConfigureAwait(false);
        return LastResult;
    }

    // ---------------------------------------------------------------- fields

    /// <summary>
    /// Adds a field. <paramref name="label"/> is what the user typed; the storage name is derived
    /// from it unless <paramref name="name"/> says otherwise. A placement puts it on the form in
    /// the same command run.
    /// </summary>
    public async Task<DesignerResult> AddFieldAsync(
        string label,
        string type,
        DesignerPlacement? placement = null,
        string? childDoctype = null,
        string? name = null,
        CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        var info = FieldTypeCatalog.Find(type)
            ?? throw new ArgumentException($"`{type}` is not a field type this designer can add.", nameof(type));
        var resolvedLabel = string.IsNullOrWhiteSpace(label) ? info.Title : label.Trim();
        var fieldName = ResolveFieldName(name, resolvedLabel, childDoctype);
        var insertAfter = placement is null ? null : LastFieldIn(placement, childDoctype);

        var result = await ApplyAsync(new AddFieldOp(fieldName, resolvedLabel, type, insertAfter, childDoctype), cancellationToken).ConfigureAwait(false);
        if (!result.Ok || placement is null)
        {
            return result;
        }

        return await ApplyAsync(new MoveInLayoutOp(fieldName, placement.SectionId, placement.AfterField, childDoctype), cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Adds a link field to an existing DocType.</summary>
    public async Task<DesignerResult> AddLinkAsync(
        string label,
        string target,
        DesignerPlacement? placement = null,
        string? name = null,
        CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrEmpty(target);
        var resolvedLabel = string.IsNullOrWhiteSpace(label) ? FieldTypeCatalog.TitleOf("link") : label.Trim();
        var fieldName = ResolveFieldName(name, resolvedLabel, null);
        var insertAfter = placement is null ? null : LastFieldIn(placement, null);
        var result = await ApplyAsync(new AddLinkOp(fieldName, resolvedLabel, target, insertAfter), cancellationToken).ConfigureAwait(false);
        if (!result.Ok || placement is null)
        {
            return result;
        }

        return await ApplyAsync(new MoveInLayoutOp(fieldName, placement.SectionId, placement.AfterField), cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Adds a child table and its child DocType, then its own grid layout.</summary>
    public async Task<DesignerResult> AddChildTableAsync(
        string label,
        string childDoctype,
        string childLabel,
        DesignerPlacement? placement = null,
        string? name = null,
        CancellationToken cancellationToken = default)
    {
        var resolvedLabel = string.IsNullOrWhiteSpace(label) ? "Child table" : label.Trim();
        var fieldName = ResolveFieldName(name, resolvedLabel, null);
        var insertAfter = placement is null ? null : LastFieldIn(placement, null);
        var result = await ApplyAsync(new AddChildTableOp(fieldName, resolvedLabel, childDoctype, childLabel, insertAfter), cancellationToken).ConfigureAwait(false);
        if (!result.Ok || placement is null)
        {
            return result;
        }

        return await ApplyAsync(new MoveInLayoutOp(fieldName, placement.SectionId, placement.AfterField), cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Renames a field's storage name.</summary>
    public Task<DesignerResult> RenameFieldAsync(string name, string to, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new RenameFieldOp(name, to, childDoctype), cancellationToken);

    /// <summary>Marks a field required, or stops requiring it.</summary>
    public Task<DesignerResult> SetRequiredAsync(string name, bool required, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetRequiredOp(name, required, childDoctype), cancellationToken);

    /// <summary>Sets (or clears) a field's default.</summary>
    public Task<DesignerResult> SetDefaultAsync(string name, string? value, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetDefaultOp(name, value, childDoctype), cancellationToken);

    /// <summary>Sets a choice list's values, in order.</summary>
    public Task<DesignerResult> SetOptionsAsync(string name, IReadOnlyList<string> options, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetOptionsOp(name, options, childDoctype), cancellationToken);

    /// <summary>Sets the depends-on rule from the builder's rows; null clears it.</summary>
    public Task<DesignerResult> SetDependsOnAsync(string name, ConditionExpression? condition, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetDependsOnOp(name, condition, childDoctype), cancellationToken);

    /// <summary>Chooses the permissions template.</summary>
    public Task<DesignerResult> SetPermissionsTemplateAsync(string template, CancellationToken cancellationToken = default)
    {
        if (PermissionsTemplates.Find(template) is null)
        {
            throw new ArgumentException($"`{template}` is not a permissions template.", nameof(template));
        }

        return ApplyAsync(new SetPermissionsTemplateOp(template), cancellationToken);
    }

    /// <summary>Sets the DocType's naming series pattern; null clears it.</summary>
    public Task<DesignerResult> SetNamingSeriesAsync(string? pattern, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetNamingSeriesOp(pattern), cancellationToken);

    /// <summary>Reorders a field in the definition (and so in the "not on the form" tray).</summary>
    public Task<DesignerResult> ReorderFieldAsync(string name, string? after, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new ReorderFieldOp(name, after, childDoctype), cancellationToken);

    /// <summary>
    /// Removes a field. The first call carries no confirmation, so the server answers with what it
    /// needs; <paramref name="confirmed"/> carries the token the server asked for.
    /// </summary>
    public Task<DesignerResult> RemoveFieldAsync(string name, bool confirmed, string? childDoctype = null, CancellationToken cancellationToken = default)
    {
        var doctype = childDoctype ?? _draft?.Doctype ?? string.Empty;
        var token = confirmed ? DesignerOpText.DataLossToken(doctype, name) : string.Empty;
        return ApplyAsync(new RemoveFieldOp(name, token, childDoctype), cancellationToken);
    }

    // ---------------------------------------------------------------- layout

    /// <summary>Puts a field into a section, optionally after another field (the drop target).</summary>
    public Task<DesignerResult> MoveInLayoutAsync(string field, string sectionId, string? after = null, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new MoveInLayoutOp(field, sectionId, after, childDoctype), cancellationToken);

    /// <summary>Adds a section at the top level or into a tab.</summary>
    public Task<DesignerResult> AddSectionAsync(string id, string title, int? columns = null, string? tabId = null, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new AddSectionOp(id, title, columns, tabId, childDoctype), cancellationToken);

    /// <summary>Adds a tab.</summary>
    public Task<DesignerResult> AddTabAsync(string id, string title, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new AddTabOp(id, title, childDoctype), cancellationToken);

    /// <summary>Sets a section's column count: the layout span the canvas resizes.</summary>
    public Task<DesignerResult> SetSectionColumnsAsync(string sectionId, int columns, string? childDoctype = null, CancellationToken cancellationToken = default) =>
        ApplyAsync(new SetSectionColumnsOp(sectionId, columns, childDoctype), cancellationToken);

    // ---------------------------------------------------------------- type changes

    /// <summary>
    /// Asks the server how a type change would treat the stored values. The answer becomes
    /// <see cref="PendingTypeChange"/>; nothing is applied until <see cref="ConfirmTypeChangeAsync"/>.
    /// </summary>
    public async Task<TypeChangePlanSummary> PlanTypeChangeAsync(string field, string newType, string? childDoctype = null, CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        PendingTypeChange = await _server.PlanTypeChangeAsync(DraftId, field, newType, childDoctype, cancellationToken).ConfigureAwait(false);
        Changed?.Invoke(this, EventArgs.Empty);
        return PendingTypeChange;
    }

    /// <summary>Applies the pending type change; the server refuses a refused classification.</summary>
    public async Task<DesignerResult> ConfirmTypeChangeAsync(CancellationToken cancellationToken = default)
    {
        if (PendingTypeChange is not { } plan)
        {
            return new DesignerResult(false, null, "There is no type change waiting.");
        }

        if (!plan.CanApply)
        {
            return new DesignerResult(false, "nocode_model.type_change_refused", plan.Summary, "Pick a compatible type instead.");
        }

        var result = await ApplyAsync(
            new ChangeTypeOp(plan.Field, plan.To, plan.Stats ?? SampleStats.Empty, null),
            cancellationToken).ConfigureAwait(false);
        if (result.Ok)
        {
            PendingTypeChange = null;
        }

        return result;
    }

    // ---------------------------------------------------------------- condition preview

    /// <summary>Asks the server to compile the visual condition, for the builder's preview line.</summary>
    public async Task<(string? Text, string? Error)> PreviewConditionAsync(ConditionExpression condition, CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        return await _server.PreviewConditionAsync(DraftId, condition, cancellationToken).ConfigureAwait(false);
    }

    // ---------------------------------------------------------------- history and lifecycle

    /// <summary>Moves the server's op-log cursor back one op.</summary>
    public async Task<DesignerResult> UndoAsync(CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        _draft = await _server.UndoAsync(DraftId, cancellationToken).ConfigureAwait(false);
        return await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Moves the server's op-log cursor forward one op.</summary>
    public async Task<DesignerResult> RedoAsync(CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        _draft = await _server.RedoAsync(DraftId, cancellationToken).ConfigureAwait(false);
        return await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>Discards the draft. A discarded draft is never published.</summary>
    public async Task<DesignerResult> DiscardAsync(CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        await _server.DiscardAsync(DraftId, cancellationToken).ConfigureAwait(false);
        _draft = await _server.OpenAsync(DraftId, cancellationToken).ConfigureAwait(false);
        return await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
    }

    // ---------------------------------------------------------------- preview and publish

    /// <summary>The preview in the requested layout: the real form's metadata, with sample data.</summary>
    public Task<PreviewModel> PreviewAsync(PreviewLayout layout, CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        return _server.PreviewAsync(DraftId, layout, cancellationToken);
    }

    /// <summary>Builds the publish plan and parks it in <see cref="PendingPublish"/>.</summary>
    public async Task<PublishPlanSummary> PlanPublishAsync(CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        PendingPublish = await _server.PlanPublishAsync(DraftId, cancellationToken).ConfigureAwait(false);
        Changed?.Invoke(this, EventArgs.Empty);
        return PendingPublish;
    }

    /// <summary>
    /// Publishes the pending plan. The server checks the <c>designer.publish</c> permission and the
    /// plan's readiness; the UI's disabled button is convenience, never the gate.
    /// </summary>
    public async Task<DesignerResult> PublishAsync(CancellationToken cancellationToken = default)
    {
        if (PendingPublish is not { } plan)
        {
            return new DesignerResult(false, null, "There is no publish plan to confirm.");
        }

        if (!plan.Ready)
        {
            return new DesignerResult(false, "nocode_model.publish_blocked", "This design cannot publish yet.", "Fix the issues the validation panel lists.");
        }

        try
        {
            _lastReceipt = await _server.PublishAsync(DraftId, plan.PlanId ?? DraftId, cancellationToken).ConfigureAwait(false);
        }
        catch (DesignerRefusalException refusal)
        {
            return DesignerResult.From(refusal.Issue);
        }

        _draft = await _server.OpenAsync(DraftId, cancellationToken).ConfigureAwait(false);
        return await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
    }

    /// <summary>The receipt of the last successful publish.</summary>
    public PublishReceipt? LastReceipt => _lastReceipt;

    // ---------------------------------------------------------------- selection

    /// <summary>Selects the element a validation issue or the canvas names.</summary>
    public void Select(ValidationIssue issue)
    {
        ArgumentNullException.ThrowIfNull(issue);
        SelectedElement = issue.Path;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Selects an element by path (`doctype`, `doctype.field` or `doctype#section`).</summary>
    public void SelectPath(string? path)
    {
        SelectedElement = path;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Searches the site's DocTypes for the link picker.</summary>
    public Task<IReadOnlyList<DocTypeChoice>> SearchDocTypesAsync(string query, CancellationToken cancellationToken = default)
    {
        if (!IsOpen)
        {
            throw new InvalidOperationException("No draft is open.");
        }

        return _server.SearchDocTypesAsync(DraftId, query ?? string.Empty, cancellationToken);
    }

    // ---------------------------------------------------------------- internals

    private async Task<DesignerResult> ApplyAsync(DesignerOp op, CancellationToken cancellationToken)
    {
        var outcome = await _server.ApplyAsync(DraftId, op, _draft?.Version ?? 0, cancellationToken).ConfigureAwait(false);
        if (!outcome.Accepted)
        {
            LastResult = outcome.Issue is null
                ? new DesignerResult(false, null, "The server did not accept that change.")
                : DesignerResult.From(outcome.Issue);
            Changed?.Invoke(this, EventArgs.Empty);
            return LastResult;
        }

        _sent.Add(op);
        _draft = outcome.Draft;
        LastResult = await AfterChangeAsync(cancellationToken).ConfigureAwait(false);
        return LastResult;
    }

    private async Task<DesignerResult> AfterChangeAsync(CancellationToken cancellationToken)
    {
        await SaveNowAsync(cancellationToken).ConfigureAwait(false);
        if (_draft is not null)
        {
            Validation = await _server.ValidateAsync(DraftId, _draft.Version, cancellationToken).ConfigureAwait(false);
        }

        Changed?.Invoke(this, EventArgs.Empty);
        return LastResult;
    }

    private async Task SaveNowAsync(CancellationToken cancellationToken)
    {
        if (_draft is null)
        {
            return;
        }

        SaveState = SaveState.Saving;
        try
        {
            await _server.SaveAsync(DraftId, _draft.Version, cancellationToken).ConfigureAwait(false);
            SaveState = SaveState.Saved;
        }
        catch (InvalidOperationException)
        {
            SaveState = SaveState.Failed;
            LastResult = new DesignerResult(false, null, "The draft could not be saved. Your last change is still on the design.");
        }
    }

    private string ResolveFieldName(string? name, string label, string? childDoctype)
    {
        var taken = _draft?.FieldsOf(childDoctype).Select(f => f.Name).ToList() ?? new List<string>();
        if (!string.IsNullOrWhiteSpace(name))
        {
            return name.Trim();
        }

        var baseName = DesignerNaming.FieldName(label, !(_draft?.NewDocType ?? true));
        return DesignerNaming.Unique(taken, baseName);
    }

    private string? LastFieldIn(DesignerPlacement placement, string? childDoctype)
    {
        if (placement.AfterField is not null)
        {
            return placement.AfterField;
        }

        var section = _draft?.LayoutOf(childDoctype).Section(placement.SectionId);
        return section is { FieldItems.Count: > 0 } ? section.FieldItems[^1] : null;
    }
}

/// <summary>The server refused a call with a friendly issue (the permission check and publish path).</summary>
public sealed class DesignerRefusalException : Exception
{
    /// <summary>Creates the exception around the server's issue.</summary>
    public DesignerRefusalException(ValidationIssue issue)
        : base(issue?.Message ?? "The server refused the request.")
    {
        Issue = issue ?? throw new ArgumentNullException(nameof(issue));
    }

    /// <summary>The server's issue.</summary>
    public ValidationIssue Issue { get; }
}
