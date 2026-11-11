namespace Spotledger.AdminCenter.Deploy;

/// <summary>A role a server can serve. The names are the engine's own vocabulary.</summary>
public enum NodeRole
{
    /// <summary>Serves the web application.</summary>
    Web,

    /// <summary>Runs background jobs.</summary>
    Jobs,

    /// <summary>Serves live updates.</summary>
    Realtime,

    /// <summary>Holds the database.</summary>
    Database,

    /// <summary>A branch-office edge node; it serves nothing else.</summary>
    BranchEdge,
}

/// <summary>The deployment profile.</summary>
public enum DeployProfile
{
    /// <summary>Everything on one server.</summary>
    Standalone,

    /// <summary>Several servers with a database primary and replicas.</summary>
    ClusterBasic,

    /// <summary>As <see cref="ClusterBasic"/> plus a shared cache for live updates.</summary>
    ClusterRedis,
}

/// <summary>How serious a pre-flight finding or a topology problem is.</summary>
public enum Severity
{
    /// <summary>Fine.</summary>
    Pass,

    /// <summary>Works, but you should look.</summary>
    Warn,

    /// <summary>Blocks the deployment.</summary>
    Fail,
}

/// <summary>Plain words for a role and a profile, so the screens never show engine names.</summary>
public static class Wording
{
    /// <summary>The label of a role.</summary>
    public static string Label(NodeRole role) => role switch
    {
        NodeRole.Web => "Web",
        NodeRole.Jobs => "Background jobs",
        NodeRole.Realtime => "Live updates",
        NodeRole.Database => "Database",
        _ => "Branch edge",
    };

    /// <summary>What a role does, in one sentence.</summary>
    public static string Explain(NodeRole role) => role switch
    {
        NodeRole.Web => "Serves the application to people's browsers and apps.",
        NodeRole.Jobs => "Runs reports, imports and other work that should not slow the screens down.",
        NodeRole.Realtime => "Pushes changes to open screens as they happen.",
        NodeRole.Database => "Stores the data. A cluster needs at least one; extra ones are replicas.",
        _ => "A small server at a branch office. It serves nothing else.",
    };

    /// <summary>The label of a profile.</summary>
    public static string Label(DeployProfile profile) => profile switch
    {
        DeployProfile.Standalone => "Single server",
        DeployProfile.ClusterBasic => "Cluster",
        _ => "Cluster with shared cache",
    };

    /// <summary>A plain explanation of a profile and when to choose it.</summary>
    public static string Explain(DeployProfile profile) => profile switch
    {
        DeployProfile.Standalone => "Everything runs on one server. Simple and cheap; if the server stops, the system stops.",
        DeployProfile.ClusterBasic => "Several servers share the work and the database is copied to a second server. Choose it when the system must stay up.",
        _ => "As a cluster, plus a shared cache so live updates reach everyone whichever web server they use. Choose it for many simultaneous users.",
    };
}

/// <summary>One server in a topology: its host name, the roles it serves, whether it is the database primary.</summary>
public sealed record ServerNode(string Host, IReadOnlySet<NodeRole> Roles, bool Primary = false);

/// <summary>The servers, their roles and the profile; the web address people will use.</summary>
public sealed record TopologyDraft(
    IReadOnlyList<ServerNode> Nodes,
    DeployProfile Profile,
    string? WebAddress);

/// <summary>One broken topology rule: what is wrong, where, and the fix in plain words.</summary>
public sealed record Violation(string Rule, IReadOnlyList<string> Hosts, string Problem, string Fix);

/// <summary>One pre-flight row: a check, its outcome and what to do about it.</summary>
public sealed record PreflightRow(string Check, string? Host, Severity Severity, string Detail, string? FixHint);

/// <summary>What the agent on a server reported about it.</summary>
public sealed record ServerFacts(
    string Host,
    string OperatingSystem,
    bool OsSupported,
    uint RamMegabytes,
    uint DataDiskFreeGigabytes,
    bool TimeSynchronised);

/// <summary>One step of a plan as the preview and the execution view show it.</summary>
public sealed record PlanStepView(
    int Index,
    string Title,
    string Host,
    IReadOnlyList<int> DependsOn,
    ulong EstimatedSeconds,
    bool NeedsCleanupBeforeRetry);

/// <summary>A deployment plan: its identity, steps, estimated time and the signatures it needs.</summary>
public sealed record PlanView(
    string PlanId,
    string Digest,
    IReadOnlyList<PlanStepView> Steps,
    ulong EstimatedTotalSeconds,
    ulong SerialSeconds,
    int RequiredSignatures);

/// <summary>A signature over a plan's digest by one approver.</summary>
public sealed record PlanSignature(string SignerId, byte[] Signature);

/// <summary>The state of one step of a run.</summary>
public enum StepStatus
{
    /// <summary>Not started.</summary>
    Pending,

    /// <summary>Running now.</summary>
    Running,

    /// <summary>Finished.</summary>
    Done,

    /// <summary>Failed; the run is stopped here.</summary>
    Failed,
}

/// <summary>One step's state with its log lines, as the journal holds it.</summary>
public sealed record StepProgress(int Index, StepStatus Status, IReadOnlyList<string> Log, string? FailureReason);

/// <summary>The whole run at one moment, from the engine's journal.</summary>
public sealed record RunSnapshot(string PlanId, IReadOnlyList<StepProgress> Steps, ulong RemainingSeconds)
{
    /// <summary>True when every step is done.</summary>
    public bool Complete => Steps.Count > 0 && Steps.All(step => step.Status == StepStatus.Done);

    /// <summary>The failed step, or null.</summary>
    public StepProgress? Failed => Steps.FirstOrDefault(step => step.Status == StepStatus.Failed);
}

/// <summary>The post-deployment health check: one row per service and the addresses people use.</summary>
public sealed record VerificationReport(IReadOnlyList<HealthRow> Health, IReadOnlyList<string> Endpoints);

/// <summary>One service's health.</summary>
public sealed record HealthRow(string Host, string Service, bool Healthy, string Detail);

/// <summary>The state of an enrolment token.</summary>
public enum EnrolmentState
{
    /// <summary>Handed out, no agent has used it.</summary>
    Waiting,

    /// <summary>An agent enrolled with it. It cannot be shown or used again.</summary>
    Used,

    /// <summary>It ran out before any agent used it.</summary>
    Expired,
}

/// <summary>An engine refusal in its own words; screens show the message, never a stack.</summary>
public sealed class DeployEngineException : Exception
{
    /// <summary>Creates the exception.</summary>
    public DeployEngineException(string message)
        : base(message)
    {
    }

    /// <summary>Creates the exception.</summary>
    public DeployEngineException()
        : base("The deployment engine refused the request.")
    {
    }

    /// <summary>Creates the exception.</summary>
    public DeployEngineException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}
