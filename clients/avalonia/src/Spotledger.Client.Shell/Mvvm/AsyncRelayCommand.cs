using System.Windows.Input;

namespace Spotledger.Client.Shell;

/// <summary>
/// A small asynchronous command with re-entrancy protection: while a run is in flight
/// <see cref="CanExecute"/> is false, so a double-click cannot log in or check twice.
/// </summary>
public sealed class AsyncRelayCommand : ICommand
{
    private readonly Func<object?, Task> _execute;
    private readonly Func<object?, bool>? _canExecute;
    private bool _isRunning;

    public AsyncRelayCommand(Func<object?, Task> execute, Func<object?, bool>? canExecute = null)
    {
        _execute = execute ?? throw new ArgumentNullException(nameof(execute));
        _canExecute = canExecute;
    }

    public event EventHandler? CanExecuteChanged;

    /// <summary>Unexpected execution failures, displayed by the owning app.</summary>
    public event Action<Exception>? ExecutionFailed;

    /// <summary>The current execution, including cleanup and error handling.</summary>
    public Task ExecutionTask { get; private set; } = Task.CompletedTask;

    /// <summary>True while a run is in flight.</summary>
    public bool IsRunning
    {
        get => _isRunning;
        private set
        {
            _isRunning = value;
            CanExecuteChanged?.Invoke(this, EventArgs.Empty);
        }
    }

    public bool CanExecute(object? parameter) => !IsRunning && (_canExecute?.Invoke(parameter) ?? true);

    public void Execute(object? parameter)
    {
        if (!CanExecute(parameter))
        {
            return;
        }

        ExecutionTask = ExecuteAsync(parameter);
    }

    private async Task ExecuteAsync(object? parameter)
    {
        IsRunning = true;
        try
        {
            await _execute(parameter);
        }
        catch (Exception exception) when (exception is OperationCanceledException or ObjectDisposedException)
        {
            // Cancellation and disposed connections are expected during shutdown.
        }
        catch (Exception exception)
        {
            ExecutionFailed?.Invoke(exception);
        }
        finally
        {
            IsRunning = false;
        }
    }

    public void RaiseCanExecuteChanged() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);
}
