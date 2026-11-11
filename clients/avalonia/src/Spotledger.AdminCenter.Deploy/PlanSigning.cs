using System.Text;
using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Deploy;

/// <summary>
/// Collects the approvers' signatures over a plan's digest. The console signs through
/// <see cref="ISigner"/>; no private key enters the app. Approvers must be distinct people.
/// </summary>
public sealed class PlanSigning : ObservableObject
{
    private readonly byte[] _payload;
    private readonly List<PlanSignature> _signatures = [];
    private string? _error;

    /// <summary>Creates the collection for a plan.</summary>
    public PlanSigning(PlanView plan)
    {
        ArgumentNullException.ThrowIfNull(plan);
        Plan = plan;
        _payload = Encoding.UTF8.GetBytes(plan.Digest);
    }

    /// <summary>The plan being signed.</summary>
    public PlanView Plan { get; }

    /// <summary>How many signatures the plan needs.</summary>
    public int Required => Plan.RequiredSignatures;

    /// <summary>The signatures collected so far.</summary>
    public IReadOnlyList<PlanSignature> Signatures => _signatures;

    /// <summary>Who has signed.</summary>
    public IReadOnlyList<string> Signers => [.. _signatures.Select(signature => signature.SignerId)];

    /// <summary>True when enough distinct approvers have signed.</summary>
    public bool IsComplete => _signatures.Count >= Required;

    /// <summary>The last refusal, in plain words.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>What the screen says about the collection.</summary>
    public string StatusText => IsComplete
        ? "Signed. The deployment can start."
        : _signatures.Count == 0
            ? $"Waiting for your signature ({Required} needed)."
            : $"Waiting for another approver ({_signatures.Count} of {Required} signed).";

    /// <summary>Signs with one approver. A second signature from the same person is refused.</summary>
    public async Task<bool> ApproveAsync(ISigner signer, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(signer);
        Error = null;
        if (IsComplete)
        {
            Error = "This plan already has every signature it needs.";
            return false;
        }

        if (_signatures.Exists(signature => string.Equals(signature.SignerId, signer.SignerId, StringComparison.Ordinal)))
        {
            Error = "The next approver must be a different person.";
            return false;
        }

        var bytes = await signer.SignAsync(_payload, cancellationToken);
        _signatures.Add(new PlanSignature(signer.SignerId, bytes));
        OnPropertyChanged(nameof(Signatures));
        OnPropertyChanged(nameof(Signers));
        OnPropertyChanged(nameof(IsComplete));
        OnPropertyChanged(nameof(StatusText));
        return true;
    }
}
