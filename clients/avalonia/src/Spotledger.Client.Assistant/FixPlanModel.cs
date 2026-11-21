namespace Spotledger.Client.Assistant;

/// <summary>The fix tier the server assigned (T1 reversible low risk, T2 data repair, T3 structural).</summary>
public enum FixTier { T1, T2, T3 }

/// <summary>How much a fix can go wrong.</summary>
public enum FixRisk { Low, Medium, High }

/// <summary>One step of a plan, as the server's fix pipeline reports it.</summary>
public sealed record FixStepView(int Index, string Id, string Target, string Operation, string Detail);

/// <summary>
/// The plan the user must see before anything changes (mirrors the server's sealed
/// <c>FixPlan</c>). <see cref="Hash"/> binds the approval: the server applies only the plan
/// whose hash the client sends back.
/// </summary>
public sealed record FixPlanView(
    string FixId,
    FixTier Tier,
    FixRisk Risk,
    bool Reversible,
    IReadOnlyList<FixStepView> Steps,
    IReadOnlyList<string> Affected,
    string Hash,
    string Summary);

/// <summary>What the server reports after applying and verifying a fix.</summary>
public sealed record FixResult(string FixId, bool Verified, string Message, long UndoDeadlineUnixMs);

/// <summary>The lifecycle of one proposed fix as the panel shows it.</summary>
public enum FixPhase
{
    Proposed,
    NeedsApproval,
    Applying,
    Applied,
    Failed,
    Undone,
    UndoExpired,
    Cancelled,
}
