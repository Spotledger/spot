namespace Spotledger.AdminCenter.Deploy;

/// <summary>
/// A one-time enrolment offer. The token and the command line and QR text that contain it are
/// secrets: the engine returns them once, the screen shows them until the token is used, and
/// nothing prints or logs them.
/// </summary>
public sealed record EnrolmentOffer(
    string OfferId,
    string Token,
    string CommandLine,
    string QrPayload,
    long ExpiresAtUnixSeconds)
{
    /// <summary>Never prints the secret.</summary>
    public override string ToString() => $"EnrolmentOffer {{ {OfferId}, secret: hidden }}";
}

/// <summary>What the engine knows about an offer: its state, and the server's facts once an agent used it.</summary>
public sealed record EnrolmentStatus(EnrolmentState State, ServerFacts? Facts);

/// <summary>
/// The seam to the deployment engine (<c>spot-deploy-wizard</c>, WP-ADM-13.T1). The Admin Center
/// host implements it over its management connection; tests use a fake. It never takes the
/// wall clock: instants are arguments.
/// </summary>
public interface IDeploymentEngine
{
    /// <summary>Creates a one-time enrolment token for a new server.</summary>
    Task<EnrolmentOffer> CreateEnrolmentAsync(string label, CancellationToken cancellationToken);

    /// <summary>Asks whether an agent has used the offer.</summary>
    Task<EnrolmentStatus> GetEnrolmentAsync(string offerId, long nowUnixSeconds, CancellationToken cancellationToken);

    /// <summary>The servers that already have an agent and can be given roles.</summary>
    Task<IReadOnlyList<ServerFacts>> ListServersAsync(CancellationToken cancellationToken);

    /// <summary>Pre-flight for the given servers (and the connections between them), as rows.</summary>
    Task<IReadOnlyList<PreflightRow>> RunPreflightAsync(IReadOnlyList<string> hosts, CancellationToken cancellationToken);

    /// <summary>Every broken topology rule at once, each with its fix. Empty when the topology is valid.</summary>
    Task<IReadOnlyList<Violation>> ValidateAsync(TopologyDraft topology, CancellationToken cancellationToken);

    /// <summary>The ordered plan with its estimate. Refused when the topology is invalid or pre-flight blocks.</summary>
    Task<PlanView> PlanAsync(TopologyDraft topology, CancellationToken cancellationToken);

    /// <summary>
    /// Runs the signed plan from its first unfinished step and streams the journal as it
    /// changes. A failed step ends the stream with that step marked failed. Calling it again
    /// after a failure (or after the app was closed) resumes at the failed step; the engine keeps
    /// the signed plan with its journal, so a resume passes no signatures.
    /// </summary>
    IAsyncEnumerable<RunSnapshot> RunAsync(string planId, IReadOnlyList<PlanSignature> signatures, CancellationToken cancellationToken);

    /// <summary>The journal as it stands, or null when the plan has not been started.</summary>
    Task<RunSnapshot?> GetJournalAsync(string planId, CancellationToken cancellationToken);

    /// <summary>The plan a run belongs to (needed to show a resumed run).</summary>
    Task<PlanView?> GetPlanAsync(string planId, CancellationToken cancellationToken);

    /// <summary>Health and endpoints after a complete run.</summary>
    Task<VerificationReport> VerifyAsync(string planId, CancellationToken cancellationToken);
}

/// <summary>Where a wizard keeps its place so it survives closing the app. It holds no secrets.</summary>
public interface IWizardStateStore
{
    /// <summary>The saved state, or null.</summary>
    Task<WizardState?> LoadAsync(CancellationToken cancellationToken);

    /// <summary>Saves the state.</summary>
    Task SaveAsync(WizardState state, CancellationToken cancellationToken);

    /// <summary>Forgets the state (the deployment finished or was abandoned).</summary>
    Task ClearAsync(CancellationToken cancellationToken);
}

/// <summary>What is saved: the topology, the plan being run and the stage.</summary>
public sealed record WizardState(TopologyDraft Topology, string? PlanId, ClusterStep Step);

/// <summary>What the wizards ask of the host shell.</summary>
public interface IDeployShell
{
    /// <summary>Puts text on the clipboard.</summary>
    Task CopyAsync(string text);
}
