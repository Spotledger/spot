namespace Spotledger.Client.Assistant;

/// <summary>A piece of context the user attaches to a question; visible and removable before sending.</summary>
public sealed record ContextChip(string Kind, string Label, string Value);

/// <summary>Where an evidence reference opens (a registered destination, never a raw URL).</summary>
public sealed record EvidenceRef(string Label, string Destination);

/// <summary>One thing the assistant looked at, with expandable evidence.</summary>
public sealed record ToolActivity(string Summary, IReadOnlyList<EvidenceRef> Evidence);

/// <summary>What the user is sending: the question plus the chips still attached.</summary>
public sealed record AssistantRequest(string Text, IReadOnlyList<ContextChip> Chips);

/// <summary>Where the AI runs, from the tenant's allowed-endpoint setting.</summary>
public sealed record AiEndpointInfo(string Endpoint, string Model, bool DataLeavesTenant);

/// <summary>The redacted bundle a user reviews before asking a human.</summary>
public sealed record DiagnosticBundle(string Text);

/// <summary>Events of a streamed reply. Assistant prose and system facts are different types.</summary>
public abstract record AssistantEvent;

/// <summary>A chunk of assistant text (limited markdown).</summary>
public sealed record TextDelta(string Text) : AssistantEvent;

/// <summary>A tool-activity entry.</summary>
public sealed record ToolEvent(ToolActivity Activity) : AssistantEvent;

/// <summary>A server-stated fact (not model prose), for example a job count.</summary>
public sealed record SystemFact(string Text) : AssistantEvent;

/// <summary>The agent proposes a fix; nothing has changed yet.</summary>
public sealed record FixProposed(FixPlanView Plan) : AssistantEvent;

/// <summary>The stream ended with the tenant's AI budget exhausted.</summary>
public sealed record BudgetStopped(string Message) : AssistantEvent;

/// <summary>The AI endpoint is unavailable; the panel falls back to the offline catalogue.</summary>
public sealed record AiUnavailable(string Reason) : AssistantEvent;

/// <summary>
/// The client's port to the server's support-agent endpoint (MCP-backed). The assistant
/// has no authority: every call runs as the signed-in user on the server.
/// </summary>
public interface IAssistantServer
{
    /// <summary>The endpoint in use.</summary>
    Task<AiEndpointInfo> GetEndpointAsync(CancellationToken ct);

    /// <summary>Streams the reply to a question.</summary>
    IAsyncEnumerable<AssistantEvent> AskAsync(AssistantRequest request, CancellationToken ct);

    /// <summary>
    /// Applies the previewed plan. <paramref name="planHash"/> binds the approval;
    /// <paramref name="typedConfirmation"/> is non-null for T2 and records what was typed.
    /// </summary>
    Task<FixResult> ApplyAsync(string fixId, string planHash, string? typedConfirmation, CancellationToken ct);

    /// <summary>Undoes an applied fix inside its undo window.</summary>
    Task<bool> UndoAsync(string fixId, CancellationToken ct);

    /// <summary>Routes a T3 plan to the approval task and returns its id.</summary>
    Task<string> RequestApprovalAsync(string fixId, string planHash, CancellationToken ct);

    /// <summary>Creates a support request carrying the (reviewed) bundle and returns its id.</summary>
    Task<string> CreateSupportRequestAsync(string summary, DiagnosticBundle bundle, CancellationToken ct);
}
