using System.Runtime.CompilerServices;

namespace Spotledger.Client.Assistant.Tests;

/// <summary>A scripted support-agent endpoint that records every call.</summary>
internal sealed class FakeServer : IAssistantServer
{
    public List<AssistantEvent> Script { get; } = [];

    public List<string> Calls { get; } = [];

    public AssistantRequest? LastRequest { get; private set; }

    public AiEndpointInfo Endpoint { get; set; } = new("https://ai.tenant.example", "tenant-model", false);

    public bool UndoSucceeds { get; set; } = true;

    public long UndoDeadline { get; set; } = 10_000;

    public bool ApplyThrows { get; set; }

    public string? LastTyped { get; private set; }

    public Task<AiEndpointInfo> GetEndpointAsync(CancellationToken ct) => Task.FromResult(Endpoint);

    public async IAsyncEnumerable<AssistantEvent> AskAsync(AssistantRequest request, [EnumeratorCancellation] CancellationToken ct)
    {
        LastRequest = request;
        Calls.Add("ask");
        foreach (var ev in Script)
        {
            ct.ThrowIfCancellationRequested();
            await Task.Yield();
            yield return ev;
        }
    }

    public Task<FixResult> ApplyAsync(string fixId, string planHash, string? typedConfirmation, CancellationToken ct)
    {
        Calls.Add("apply:" + planHash);
        LastTyped = typedConfirmation;
        if (ApplyThrows) { throw new HttpRequestException("https://x.example/?token=SECRET"); }
        return Task.FromResult(new FixResult(fixId, true, "Verified: queue is moving.", UndoDeadline));
    }

    public Task<bool> UndoAsync(string fixId, CancellationToken ct)
    {
        Calls.Add("undo");
        return Task.FromResult(UndoSucceeds);
    }

    public Task<string> RequestApprovalAsync(string fixId, string planHash, CancellationToken ct)
    {
        Calls.Add("approval");
        return Task.FromResult("task-77");
    }

    public Task<string> CreateSupportRequestAsync(string summary, DiagnosticBundle bundle, CancellationToken ct)
    {
        Calls.Add("support:" + bundle.Text);
        return Task.FromResult("SR-1");
    }

    public static FixPlanView CachePlan() => new(
        "fix-1", FixTier.T1, FixRisk.Low, true,
        [new FixStepView(0, "s1", "cache:items", "cache.flush", "Flush the item cache.")],
        ["cache:items"], "hash-1", "Flush the stale cache.");

    public static FixPlanView RetryPlan() => new(
        "fix-2", FixTier.T2, FixRisk.Medium, true,
        [new FixStepView(0, "s1", "job:17", "job.retry", "Retry job 17."), new FixStepView(1, "s2", "job:18", "job.retry", "Retry job 18.")],
        ["job:17", "job:18"], "hash-2", "Retry the stuck jobs.");

    public static FixPlanView StructuralPlan() => new(
        "fix-3", FixTier.T3, FixRisk.High, true,
        [new FixStepView(0, "s1", "meta:party", "customise.apply", "Apply the field change.")],
        ["meta:party"], "hash-3", "Change the party form.");
}
