using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Tenants;

/// <summary>What every tenant and fleet screen works over.</summary>
public sealed record TenantContext(ITenantsApi Api, EquivalentCommand Commands);

/// <summary>
/// One action waiting for its signatures: it states what will happen, shows the equivalent
/// command, collects the signatures the operation's risk needs and only then runs. An operation the
/// catalogue does not know fails closed.
/// </summary>
public sealed class ActionGate : ObservableObject
{
    private readonly ITenantsApi _api;
    private string? _error;
    private bool _done;

    /// <summary>Creates the gate for an action.</summary>
    public ActionGate(ITenantsApi api, EquivalentCommand commands, AdminAction action, string summary)
    {
        _api = api ?? throw new ArgumentNullException(nameof(api));
        ArgumentNullException.ThrowIfNull(commands);
        Action = action ?? throw new ArgumentNullException(nameof(action));
        Summary = summary;
        try
        {
            Signing = new SigningSession(action, commands);
            EquivalentCommandText = commands.Cli(action);
        }
        catch (KeyNotFoundException unknown)
        {
            _error = unknown.Message;
        }
    }

    /// <summary>The action.</summary>
    public AdminAction Action { get; }

    /// <summary>What will happen, in a sentence.</summary>
    public string Summary { get; }

    /// <summary>The signing session; null when the operation is not in the catalogue.</summary>
    public SigningSession? Signing { get; }

    /// <summary>The command line the person can copy.</summary>
    public string? EquivalentCommandText { get; }

    /// <summary>True when the signatures needed are in.</summary>
    public bool IsReady => !_done && Signing is { State: SigningState.NotRequired or SigningState.Complete };

    /// <summary>True once the action ran.</summary>
    public bool IsDone
    {
        get => _done;
        private set => SetField(ref _done, value);
    }

    /// <summary>The last refusal.</summary>
    public string? Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>Signs with one approver; the same person cannot sign twice.</summary>
    public async Task<bool> ApproveAsync(ISigner signer, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(signer);
        if (Signing is null)
        {
            return false;
        }

        try
        {
            await Signing.ApproveAsync(signer, cancellationToken);
        }
        catch (InvalidOperationException refused)
        {
            Error = refused.Message;
            return false;
        }

        Error = null;
        OnPropertyChanged(nameof(IsReady));
        return true;
    }

    /// <summary>Runs the action. Refused until the signatures are in.</summary>
    public async Task<bool> ExecuteAsync(CancellationToken cancellationToken = default)
    {
        if (!IsReady || Signing is null)
        {
            Error ??= "The signatures this action needs are not complete.";
            return false;
        }

        try
        {
            await _api.ExecuteAsync(Action, Signing.Approvals, cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        Error = null;
        IsDone = true;
        OnPropertyChanged(nameof(IsReady));
        return true;
    }
}

/// <summary>The base of the tenant and fleet screens: loading, the last error, and the action waiting for signatures.</summary>
public abstract class TenantScreen : ObservableObject
{
    private string? _error;
    private bool _isLoading;
    private ActionGate? _gate;

    /// <summary>Creates the screen.</summary>
    protected TenantScreen(TenantContext context)
    {
        Context = context ?? throw new ArgumentNullException(nameof(context));
    }

    /// <summary>The tool id (see <see cref="TenantTools"/>).</summary>
    public abstract string Id { get; }

    /// <summary>The title.</summary>
    public abstract string Title { get; }

    /// <summary>The order the keyboard visits the controls, for focus handling and tests.</summary>
    public abstract IReadOnlyList<string> FocusOrder { get; }

    /// <summary>The context.</summary>
    protected TenantContext Context { get; }

    /// <summary>The management API.</summary>
    protected ITenantsApi Api => Context.Api;

    /// <summary>The last failure in plain words.</summary>
    public string? Error
    {
        get => _error;
        protected set => SetField(ref _error, value);
    }

    /// <summary>True while data loads.</summary>
    public bool IsLoading
    {
        get => _isLoading;
        private set => SetField(ref _isLoading, value);
    }

    /// <summary>The action waiting for signatures; null when none.</summary>
    public ActionGate? Gate
    {
        get => _gate;
        private set => SetField(ref _gate, value);
    }

    /// <summary>Loads the screen. <paramref name="nowUnixSeconds"/> is the caller's clock.</summary>
    public async Task<bool> LoadAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        IsLoading = true;
        Error = null;
        try
        {
            await FetchAsync(nowUnixSeconds, cancellationToken);
            return true;
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }
        finally
        {
            IsLoading = false;
        }
    }

    /// <summary>Runs the waiting action once its signatures are in, then reloads.</summary>
    public async Task<bool> ConfirmAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Gate is not { } gate)
        {
            return false;
        }

        if (!await gate.ExecuteAsync(cancellationToken))
        {
            Error = gate.Error;
            return false;
        }

        Gate = null;
        await LoadAsync(nowUnixSeconds, cancellationToken);
        return true;
    }

    /// <summary>Drops the waiting action.</summary>
    public void Cancel() => Gate = null;

    /// <summary>Fetches the screen's data.</summary>
    protected abstract Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken);

    /// <summary>Starts an action: it waits for its signatures, nothing is sent yet.</summary>
    protected ActionGate Begin(AdminAction action, string summary)
    {
        Error = null;
        Gate = new ActionGate(Api, Context.Commands, action, summary);
        if (Gate.Error is { } failure)
        {
            Error = failure;
        }

        return Gate;
    }
}
