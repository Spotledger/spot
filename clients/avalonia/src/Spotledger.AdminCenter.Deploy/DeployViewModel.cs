using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Deploy;

/// <summary>The base of the deployment screens: a busy flag, the last error in plain words and a status line.</summary>
public abstract class DeployViewModel : ObservableObject
{
    private string? _error;
    private string? _status;
    private bool _isBusy;

    /// <summary>Creates the view-model over the engine.</summary>
    protected DeployViewModel(IDeploymentEngine engine)
    {
        Engine = engine ?? throw new ArgumentNullException(nameof(engine));
    }

    /// <summary>The deployment engine.</summary>
    protected IDeploymentEngine Engine { get; }

    /// <summary>The engine's last refusal, shown inline; null when the last action worked.</summary>
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

    /// <summary>True while an engine call is running.</summary>
    public bool IsBusy
    {
        get => _isBusy;
        protected set => SetField(ref _isBusy, value);
    }

    /// <summary>
    /// Runs an engine call: clears the error, keeps <see cref="IsBusy"/> honest and, on a refusal,
    /// shows the engine's message and returns false.
    /// </summary>
    protected async Task<bool> TryAsync(Func<Task> call)
    {
        Error = null;
        IsBusy = true;
        try
        {
            await call();
            return true;
        }
        catch (DeployEngineException refused)
        {
            Error = refused.Message;
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }
}
