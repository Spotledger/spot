using System.Security.Cryptography;
using System.Text;

namespace Spotledger.AdminCenter;

/// <summary>
/// Signs a payload on behalf of one administrator. The private key never enters the app:
/// FIDO2/PIV implementations arrive later behind this abstraction.
/// </summary>
public interface ISigner
{
    string SignerId { get; }

    Task<byte[]> SignAsync(byte[] payload, CancellationToken cancellationToken);
}

public enum SigningState
{
    NotRequired,
    AwaitingFirstApproval,
    AwaitingSecondApprover,
    Complete,
}

public sealed record Approval(string SignerId, byte[] Signature);

/// <summary>
/// The signing flow for one action: collects distinct approvers until the operation's
/// requirement is met (two-person rule), and tells the UI what to say meanwhile.
/// </summary>
public sealed class SigningSession
{
    private readonly byte[] _payload;
    private readonly List<Approval> _approvals = [];

    public SigningSession(AdminAction action, EquivalentCommand commands)
    {
        ArgumentNullException.ThrowIfNull(action);
        ArgumentNullException.ThrowIfNull(commands);
        Action = action;
        Required = commands.Spec(action.OperationId).RequiredApprovals;
        _payload = Encoding.UTF8.GetBytes(commands.SignedCommandPreview(action));
    }

    public AdminAction Action { get; }

    public int Required { get; }

    public IReadOnlyList<Approval> Approvals => _approvals;

    public SigningState State =>
        Required == 0 ? SigningState.NotRequired
        : _approvals.Count >= Required ? SigningState.Complete
        : _approvals.Count == 0 ? SigningState.AwaitingFirstApproval
        : SigningState.AwaitingSecondApprover;

    public string StatusText => State switch
    {
        SigningState.NotRequired => "No signature required",
        SigningState.AwaitingFirstApproval => "Waiting for your signature",
        SigningState.AwaitingSecondApprover => "Waiting for second approver",
        _ => "Signed",
    };

    public async Task ApproveAsync(ISigner signer, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(signer);
        if (State is SigningState.NotRequired or SigningState.Complete)
        {
            throw new InvalidOperationException("This action needs no further signatures.");
        }

        if (_approvals.Exists(a => string.Equals(a.SignerId, signer.SignerId, StringComparison.Ordinal)))
        {
            throw new InvalidOperationException("The second approver must be a different person.");
        }

        var signature = await signer.SignAsync(_payload, cancellationToken).ConfigureAwait(false);
        _approvals.Add(new Approval(signer.SignerId, signature));
    }
}

/// <summary>A software signer for tests and demos only: an HMAC over the payload with a caller-supplied key.</summary>
public sealed class SoftwareTestSigner : ISigner
{
    private readonly byte[] _key;

    public SoftwareTestSigner(string signerId, byte[] key)
    {
        SignerId = signerId;
        _key = key;
    }

    public string SignerId { get; }

    public Task<byte[]> SignAsync(byte[] payload, CancellationToken cancellationToken) =>
        Task.FromResult(HMACSHA256.HashData(_key, payload));
}
