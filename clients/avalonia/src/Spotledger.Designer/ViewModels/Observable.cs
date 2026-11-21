using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows.Input;

namespace Spotledger.Client.Designer;

/// <summary>Minimal MVVM base: no framework package, just <see cref="INotifyPropertyChanged"/>.</summary>
public abstract class ObservableObject : INotifyPropertyChanged
{
    /// <inheritdoc />
    public event PropertyChangedEventHandler? PropertyChanged;

    /// <summary>Raises <see cref="PropertyChanged"/>.</summary>
    protected void OnPropertyChanged([CallerMemberName] string? propertyName = null) =>
        PropertyChanged?.Invoke(this, new PropertyChangedEventArgs(propertyName));

    /// <summary>Sets a field and raises <see cref="PropertyChanged"/> when it changed.</summary>
    protected bool SetField<T>(ref T field, T value, [CallerMemberName] string? propertyName = null)
    {
        if (EqualityComparer<T>.Default.Equals(field, value))
        {
            return false;
        }

        field = value;
        OnPropertyChanged(propertyName);
        return true;
    }
}

/// <summary>A small synchronous command, hand-written so the designer takes no MVVM package.</summary>
public sealed class RelayCommand : ICommand
{
    private readonly Action<object?> _execute;
    private readonly Func<object?, bool>? _canExecute;

    /// <summary>Creates the command.</summary>
    public RelayCommand(Action<object?> execute, Func<object?, bool>? canExecute = null)
    {
        _execute = execute ?? throw new ArgumentNullException(nameof(execute));
        _canExecute = canExecute;
    }

    /// <inheritdoc />
    public event EventHandler? CanExecuteChanged;

    /// <inheritdoc />
    public bool CanExecute(object? parameter) => _canExecute?.Invoke(parameter) ?? true;

    /// <inheritdoc />
    public void Execute(object? parameter) => _execute(parameter);

    /// <summary>Asks every bound control to re-check <see cref="CanExecute"/>.</summary>
    public void RaiseCanExecuteChanged() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);
}

/// <summary>
/// A small asynchronous command with re-entrancy protection: while a run is in flight
/// <see cref="CanExecute"/> is false, so a double action cannot send the same op twice.
/// </summary>
public sealed class AsyncRelayCommand : ICommand
{
    private readonly Func<object?, Task> _execute;
    private readonly Func<object?, bool>? _canExecute;
    private bool _isRunning;

    /// <summary>Creates the command.</summary>
    public AsyncRelayCommand(Func<object?, Task> execute, Func<object?, bool>? canExecute = null)
    {
        _execute = execute ?? throw new ArgumentNullException(nameof(execute));
        _canExecute = canExecute;
    }

    /// <inheritdoc />
    public event EventHandler? CanExecuteChanged;

    /// <summary>Unexpected execution failures, displayed by the owning view.</summary>
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

    /// <inheritdoc />
    public bool CanExecute(object? parameter) => !IsRunning && (_canExecute?.Invoke(parameter) ?? true);

    /// <inheritdoc />
    public void Execute(object? parameter)
    {
        if (!CanExecute(parameter))
        {
            return;
        }

        ExecutionTask = ExecuteAsync(parameter);
    }

    /// <summary>Asks every bound control to re-check <see cref="CanExecute"/>.</summary>
    public void RaiseCanExecuteChanged() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);

    private async Task ExecuteAsync(object? parameter)
    {
        IsRunning = true;
        try
        {
            await _execute(parameter).ConfigureAwait(true);
        }
        catch (OperationCanceledException)
        {
            // Shutdown cancels in-flight work; nothing to show.
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
}
