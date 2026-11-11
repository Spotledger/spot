using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// The base of the identity screens: a busy flag, the last server error in its own words, a
/// status line and the step-up retry that every sensitive action shares.
/// </summary>
public abstract class IdentityViewModel : ObservableObject
{
    private string? _error;
    private string? _status;
    private bool _isBusy;
    private OneTimeSecret? _secret;

    /// <summary>Creates the view-model over a gateway and a shell.</summary>
    protected IdentityViewModel(IIdentityGateway gateway, IIdentityShell shell)
    {
        Gateway = gateway ?? throw new ArgumentNullException(nameof(gateway));
        Shell = shell ?? throw new ArgumentNullException(nameof(shell));
    }

    /// <summary>The server.</summary>
    protected IIdentityGateway Gateway { get; }

    /// <summary>The dialogs.</summary>
    protected IIdentityShell Shell { get; }

    /// <summary>The server's last error message, shown inline; null when the last action worked.</summary>
    public string? Error
    {
        get => _error;
        protected set => SetField(ref _error, value);
    }

    /// <summary>A short note about the last action.</summary>
    public string? Status
    {
        get => _status;
        protected set => SetField(ref _status, value);
    }

    /// <summary>True while a server call is running.</summary>
    public bool IsBusy
    {
        get => _isBusy;
        protected set => SetField(ref _isBusy, value);
    }

    /// <summary>
    /// A secret the server returned once, for the screen to show with a copy button. It is not
    /// kept anywhere else and <see cref="DismissSecret"/> clears it.
    /// </summary>
    public OneTimeSecret? RevealedSecret
    {
        get => _secret;
        private set => SetField(ref _secret, value);
    }

    /// <summary>Hides and forgets the one-time secret.</summary>
    public void DismissSecret() => RevealedSecret = null;

    /// <summary>Shows a one-time secret.</summary>
    protected void Reveal(OneTimeSecret secret) => RevealedSecret = secret;

    /// <summary>
    /// Runs a gateway call: clears the error, keeps <see cref="IsBusy"/> honest and, on failure,
    /// shows the server's message and returns null.
    /// </summary>
    protected async Task<T?> CallAsync<T>(Func<Task<GatewayResult<T>>> call)
    {
        Error = null;
        IsBusy = true;
        try
        {
            var result = await call();
            if (!result.IsOk)
            {
                Error = result.Error?.Message;
                return default;
            }

            return result.Value;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>Like <see cref="CallAsync{T}"/> for a call whose value is not needed (or is a value type): true when it worked.</summary>
    protected async Task<bool> TryAsync<T>(Func<Task<GatewayResult<T>>> call)
    {
        Error = null;
        IsBusy = true;
        try
        {
            var result = await call();
            if (!result.IsOk)
            {
                Error = result.Error?.Message;
                return false;
            }

            return true;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>
    /// Runs a call that needs a step-up: the first try carries no proof; if the server asks for a
    /// step-up the person is prompted once and the call is retried with the proof. Declining the
    /// prompt cancels with no error.
    /// </summary>
    protected async Task<bool> WithStepUpAsync(string reason, Func<StepUpProof?, Task<GatewayResult<Unit>>> call)
    {
        Error = null;
        IsBusy = true;
        try
        {
            var result = await call(null);
            if (!result.IsOk && result.Error?.Code == GatewayError.StepUpRequired)
            {
                var proof = await Shell.PromptStepUpAsync(reason);
                if (proof is null)
                {
                    Status = "Cancelled.";
                    return false;
                }

                result = await call(proof);
            }

            if (!result.IsOk)
            {
                Error = result.Error?.Message;
                return false;
            }

            return true;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <summary>Like <see cref="WithStepUpAsync"/> for a call that returns a value.</summary>
    protected async Task<T?> WithStepUpAsync<T>(string reason, Func<StepUpProof?, Task<GatewayResult<T>>> call)
    {
        Error = null;
        IsBusy = true;
        try
        {
            var result = await call(null);
            if (!result.IsOk && result.Error?.Code == GatewayError.StepUpRequired)
            {
                var proof = await Shell.PromptStepUpAsync(reason);
                if (proof is null)
                {
                    Status = "Cancelled.";
                    return default;
                }

                result = await call(proof);
            }

            if (!result.IsOk)
            {
                Error = result.Error?.Message;
                return default;
            }

            return result.Value;
        }
        finally
        {
            IsBusy = false;
        }
    }
}
