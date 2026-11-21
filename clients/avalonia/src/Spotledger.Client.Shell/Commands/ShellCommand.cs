using System.Windows.Input;

namespace Spotledger.Client.Shell;

/// <summary>
/// One command as the chrome binds it: menu items, the launcher and the status bar read
/// the same object, enabled exactly when the dispatcher would run it, with the reason the
/// dispatcher would give. Clicks always go through <see cref="CommandDispatcher"/>; this
/// class holds no behaviour of its own.
/// </summary>
public sealed class ShellCommand : ObservableObject, ICommand
{
    private readonly ShellViewModel _owner;

    public ShellCommand(CommandDefinition definition, ShellViewModel owner)
    {
        Definition = definition ?? throw new ArgumentNullException(nameof(definition));
        _owner = owner ?? throw new ArgumentNullException(nameof(owner));
        _owner.CommandsStateChanged += (_, _) =>
        {
            OnPropertyChanged(nameof(IsAvailable));
            OnPropertyChanged(nameof(Reason));
            CanExecuteChanged?.Invoke(this, EventArgs.Empty);
        };
    }

    public CommandDefinition Definition { get; }

    public string Id => Definition.Id;

    public string Label => _owner.LabelOf(Definition);

    /// <summary>The header with the Avalonia access-key underscore (e.g. "_Close tab").</summary>
    public string Header => Definition.Mnemonic is ' '
        ? Label
        : Views.MenuBarBuilder.AccessKey(Label, Definition.Mnemonic);

    /// <summary>Shortcut text from the active keymap for the menu column; null when unbound.</summary>
    public string? GestureText => _owner.Keymap.Find(Definition.Id) is { } binding ? binding.Chord : null;

    /// <summary>The command's launcher group chip (Create / Open / Reports / Recent).</summary>
    public string Category => Definition.Category;

    /// <summary>True when the dispatcher would run this command right now.</summary>
    public bool IsAvailable => _owner.Dispatcher.GetAvailability(Id, _owner.CurrentContext()).IsAvailable;

    /// <summary>Why the command cannot run (shown as the greyed item's tooltip and the
    /// launcher caption); null when available.</summary>
    public string? Reason => _owner.Dispatcher.GetAvailability(Id, _owner.CurrentContext()).Reason;

    public event EventHandler? CanExecuteChanged;

    public bool CanExecute(object? parameter) => IsAvailable;

    public void Execute(object? parameter)
    {
        // Fire-and-observe: RunAsync dedupes a re-trigger and reports a refused run's
        // reason on the status bar. Route from parameter (diagnostic only).
        var route = parameter is CommandRoute parsed ? parsed : CommandRoute.Menu;
        _ = _owner.RunAsync(Id, route);
    }

    /// <summary>Raised by the shell when availability may have changed.</summary>
    public void RaiseCanExecuteChanged() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);

    /// <summary>Raised when the active keymap profile changed so the gesture column refreshes.</summary>
    public void NotifyGestureChanged()
    {
        OnPropertyChanged(nameof(GestureText));
        CanExecuteChanged?.Invoke(this, EventArgs.Empty);
    }
}
