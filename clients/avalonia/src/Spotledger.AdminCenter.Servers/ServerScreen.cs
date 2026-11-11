using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Servers;

/// <summary>What every server screen works over: the connected server's API, the command catalogue and its capabilities.</summary>
public sealed record ServerContext(IServerApi Api, EquivalentCommand Commands, ServerCapabilities Capabilities);

/// <summary>
/// The base of the server screens: load state, the last error in plain words, and the single
/// operation in flight. Every state-changing request goes through <see cref="RequestAsync"/>, which
/// shows the plan preview first; nothing is sent to the server before the person reviews it.
/// </summary>
public abstract class ServerScreen : ObservableObject
{
    private string? _error;
    private bool _isLoading;
    private OperationRunner? _operation;

    /// <summary>Creates the screen.</summary>
    protected ServerScreen(ServerContext context)
    {
        Context = context ?? throw new ArgumentNullException(nameof(context));
    }

    /// <summary>The tool id (see <see cref="ServerTools"/>).</summary>
    public abstract string Id { get; }

    /// <summary>The title.</summary>
    public abstract string Title { get; }

    /// <summary>The order the keyboard visits the controls, for focus handling and tests.</summary>
    public abstract IReadOnlyList<string> FocusOrder { get; }

    /// <summary>The server context.</summary>
    protected ServerContext Context { get; }

    /// <summary>The management API.</summary>
    protected IServerApi Api => Context.Api;

    /// <summary>The capabilities the agent reported.</summary>
    protected ServerCapabilities Capabilities => Context.Capabilities;

    /// <summary>The last failure, in plain words.</summary>
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

    /// <summary>The operation in flight (preview, signatures, progress, result); null when none.</summary>
    public OperationRunner? Operation
    {
        get => _operation;
        private set => SetField(ref _operation, value);
    }

    /// <summary>Loads the screen's data. <paramref name="nowUnixSeconds"/> is the caller's clock.</summary>
    public async Task<bool> LoadAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        IsLoading = true;
        Error = null;
        try
        {
            await FetchAsync(nowUnixSeconds, cancellationToken);
            return true;
        }
        catch (ServerApiException refused)
        {
            Error = refused.Message;
            return false;
        }
        finally
        {
            IsLoading = false;
        }
    }

    /// <summary>Runs the pending operation (after its preview, warnings and signatures) and reloads the screen when it worked.</summary>
    public async Task<bool> RunOperationAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (Operation is not { } operation)
        {
            return false;
        }

        var ok = await operation.RunAsync(cancellationToken);
        if (ok && operation.Phase == OperationPhase.Succeeded)
        {
            await LoadAsync(nowUnixSeconds, cancellationToken);
        }

        return ok;
    }

    /// <summary>Drops the pending operation.</summary>
    public void DismissOperation()
    {
        Operation?.Cancel();
        Operation = null;
    }

    /// <summary>Fetches the screen's data.</summary>
    protected abstract Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken);

    /// <summary>
    /// Starts an operation: asks the module for its plan and shows it. Refused (with a reason) when
    /// the agent did not report the operation, so a button never offers what the server lacks.
    /// </summary>
    protected async Task<bool> RequestAsync(AdminAction action, CancellationToken cancellationToken)
    {
        if (!Capabilities.Supports(action.OperationId))
        {
            Error = "This server does not support that action.";
            return false;
        }

        Error = null;
        var runner = new OperationRunner(Api, Context.Commands, action);
        Operation = runner;
        await runner.PrepareAsync(cancellationToken);
        if (runner.Phase == OperationPhase.NothingToDo)
        {
            Error = "Nothing to change: the server is already in that state.";
        }
        else if (runner.Error is { } failure)
        {
            Error = failure;
        }

        return runner.Phase == OperationPhase.Review;
    }
}
