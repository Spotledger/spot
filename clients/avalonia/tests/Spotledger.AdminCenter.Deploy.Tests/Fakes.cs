using System.Runtime.CompilerServices;
using Spotledger.AdminCenter;
using Spotledger.AdminCenter.Deploy;

namespace Spotledger.AdminCenter.Deploy.Tests;

internal sealed class FakeShell : IDeployShell
{
    public List<string> Copied { get; } = [];

    public Task CopyAsync(string text)
    {
        Copied.Add(text);
        return Task.CompletedTask;
    }
}

internal sealed class MemoryStore : IWizardStateStore
{
    public WizardState? State { get; set; }

    public int Saves { get; private set; }

    public Task<WizardState?> LoadAsync(CancellationToken cancellationToken) => Task.FromResult(State);

    public Task SaveAsync(WizardState state, CancellationToken cancellationToken)
    {
        State = state;
        Saves++;
        return Task.CompletedTask;
    }

    public Task ClearAsync(CancellationToken cancellationToken)
    {
        State = null;
        return Task.CompletedTask;
    }
}

internal sealed class FakeSigner(string id) : ISigner
{
    public string SignerId { get; } = id;

    public Task<byte[]> SignAsync(byte[] payload, CancellationToken cancellationToken) =>
        Task.FromResult(System.Text.Encoding.UTF8.GetBytes($"{SignerId}:{payload.Length}"));
}

/// <summary>
/// An in-memory engine with the engine's rules in miniature: validation reports every violation at
/// once, pre-flight rows come from the servers' facts, a plan has one step per server and role, and
/// a run can be made to fail at one step and then resume where it stopped.
/// </summary>
internal sealed class FakeEngine : IDeploymentEngine
{
    private readonly Dictionary<string, List<StepProgress>> _journals = [];
    private readonly Dictionary<string, PlanView> _plans = [];

    public List<ServerFacts> Servers { get; } = [];

    public List<string> Calls { get; } = [];

    public EnrolmentStatus Enrolment { get; set; } = new(EnrolmentState.Waiting, null);

    public int Token { get; private set; }

    public int? FailStep { get; set; }

    public int? StopAfterStep { get; set; }

    public Dictionary<string, Severity> PreflightFor { get; } = [];

    public int RequiredSignatures { get; set; } = 2;

    public string? Refuse { get; set; }

    public List<IReadOnlyList<PlanSignature>> RunSignatures { get; } = [];

    public int ValidationDelayRounds { get; set; }

    public Task<EnrolmentOffer> CreateEnrolmentAsync(string label, CancellationToken cancellationToken)
    {
        Calls.Add("CreateEnrolment");
        Token++;
        return Task.FromResult(new EnrolmentOffer(
            $"offer-{Token}",
            $"tok-secret-{Token}",
            $"spot-agent enrol --token tok-secret-{Token}",
            $"spot://enrol?t=tok-secret-{Token}",
            ExpiresAtUnixSeconds: 1_000));
    }

    public Task<EnrolmentStatus> GetEnrolmentAsync(string offerId, long nowUnixSeconds, CancellationToken cancellationToken)
    {
        Calls.Add("GetEnrolment");
        return Task.FromResult(Enrolment);
    }

    public Task<IReadOnlyList<ServerFacts>> ListServersAsync(CancellationToken cancellationToken) =>
        Task.FromResult<IReadOnlyList<ServerFacts>>([.. Servers]);

    public Task<IReadOnlyList<PreflightRow>> RunPreflightAsync(IReadOnlyList<string> hosts, CancellationToken cancellationToken)
    {
        Calls.Add("Preflight");
        var rows = new List<PreflightRow>();
        foreach (var host in hosts)
        {
            var severity = PreflightFor.GetValueOrDefault(host, Severity.Pass);
            rows.Add(new PreflightRow("Operating system", host, Severity.Pass, "Ubuntu 24.04", null));
            rows.Add(new PreflightRow(
                "Free disk",
                host,
                severity,
                severity == Severity.Pass ? "120 GB free" : "12 GB free",
                severity == Severity.Pass ? null : "Free up space or attach a bigger data disk (at least 20 GB)."));
        }

        return Task.FromResult<IReadOnlyList<PreflightRow>>(rows);
    }

    public Task<IReadOnlyList<Violation>> ValidateAsync(TopologyDraft topology, CancellationToken cancellationToken)
    {
        Calls.Add("Validate");
        var violations = new List<Violation>();
        var databases = topology.Nodes.Where(n => n.Roles.Contains(NodeRole.Database)).ToList();
        if (databases.Count == 0)
        {
            violations.Add(new Violation("database-required", [], "No server holds the database.", "Give the Database role to one server."));
        }

        if (!topology.Nodes.Any(n => n.Roles.Contains(NodeRole.Web)))
        {
            violations.Add(new Violation("web-required", [], "No server serves the web application.", "Give the Web role to a server."));
        }

        foreach (var edge in topology.Nodes.Where(n => n.Roles.Contains(NodeRole.BranchEdge) && n.Roles.Count > 1))
        {
            violations.Add(new Violation("branch-edge-alone", [edge.Host], $"{edge.Host} is a branch edge and also serves other roles.", "Keep the branch edge role on its own server."));
        }

        if (topology.Nodes.Any(n => n.Roles.Contains(NodeRole.Web)) && string.IsNullOrWhiteSpace(topology.WebAddress))
        {
            violations.Add(new Violation("web-address", [], "No address is set for the web servers.", "Enter the address people will use."));
        }

        return Task.FromResult<IReadOnlyList<Violation>>(violations);
    }

    public Task<PlanView> PlanAsync(TopologyDraft topology, CancellationToken cancellationToken)
    {
        Calls.Add("Plan");
        if (Refuse is { } reason)
        {
            throw new DeployEngineException(reason);
        }

        var steps = new List<PlanStepView>();
        var index = 0;
        foreach (var node in topology.Nodes)
        {
            steps.Add(new PlanStepView(index++, "Enrol agent", node.Host, [], 60, false));
            steps.Add(new PlanStepView(index++, "Harden baseline", node.Host, [index - 2], 120, false));
            foreach (var role in node.Roles.OrderBy(r => r))
            {
                steps.Add(new PlanStepView(index++, $"Install {Wording.Label(role)}", node.Host, [index - 2], 300, role == NodeRole.Database));
            }
        }

        var plan = new PlanView("plan-1", "digest-of-plan-1", steps, 900, (ulong)steps.Sum(s => (long)s.EstimatedSeconds), RequiredSignatures);
        _plans[plan.PlanId] = plan;
        return Task.FromResult(plan);
    }

    public async IAsyncEnumerable<RunSnapshot> RunAsync(string planId, IReadOnlyList<PlanSignature> signatures, [EnumeratorCancellation] CancellationToken cancellationToken)
    {
        Calls.Add("Run");
        RunSignatures.Add(signatures);
        if (!_journals.TryGetValue(planId, out var journal))
        {
            if (signatures.Count < RequiredSignatures)
            {
                throw new DeployEngineException("The plan is not fully signed.");
            }

            journal = [.. _plans[planId].Steps.Select(s => new StepProgress(s.Index, StepStatus.Pending, [], null))];
            _journals[planId] = journal;
        }

        for (var i = 0; i < journal.Count; i++)
        {
            if (journal[i].Status == StepStatus.Done)
            {
                continue;
            }

            journal[i] = new StepProgress(i, StepStatus.Running, [$"step {i} started"], null);
            yield return Snapshot(planId);
            await Task.Yield();
            if (FailStep == i)
            {
                FailStep = null;
                journal[i] = new StepProgress(i, StepStatus.Failed, [$"step {i} started", "connection reset"], "The server stopped answering.");
                yield return Snapshot(planId);
                yield break;
            }

            journal[i] = new StepProgress(i, StepStatus.Done, [$"step {i} started", $"step {i} finished"], null);
            yield return Snapshot(planId);
            if (StopAfterStep == i)
            {
                StopAfterStep = null;
                yield break;
            }
        }
    }

    public Task<RunSnapshot?> GetJournalAsync(string planId, CancellationToken cancellationToken) =>
        Task.FromResult(_journals.ContainsKey(planId) ? Snapshot(planId) : null);

    public Task<PlanView?> GetPlanAsync(string planId, CancellationToken cancellationToken) =>
        Task.FromResult(_plans.GetValueOrDefault(planId));

    public Task<VerificationReport> VerifyAsync(string planId, CancellationToken cancellationToken)
    {
        Calls.Add("Verify");
        return Task.FromResult(new VerificationReport(
            [.. Servers.Select(s => new HealthRow(s.Host, "agent", true, "answering"))],
            ["https://erp.example.com"]));
    }

    private RunSnapshot Snapshot(string planId)
    {
        var journal = _journals[planId];
        var remaining = (ulong)journal.Count(p => p.Status != StepStatus.Done) * 60;
        return new RunSnapshot(planId, [.. journal], remaining);
    }

    public static ServerFacts Facts(string host) => new(host, "Ubuntu 24.04 LTS", true, 16_384, 200, true);
}
