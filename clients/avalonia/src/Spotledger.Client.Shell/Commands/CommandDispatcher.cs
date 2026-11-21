namespace Spotledger.Client.Shell;

/// <summary>Where a command invocation came from. Diagnostic only: every route runs the
/// same availability check and the same handler, so all routes behave identically.</summary>
public enum CommandRoute
{
    Menu,
    Launcher,
    Shortcut,
    ContextMenu,
}

/// <summary>The state a command runs against. The shell fills this from local view-model
/// state today; the server seam supplies the same shape later (ADR-055).</summary>
/// <param name="ActiveTab">The tab the command acts on, when one is required.</param>
/// <param name="HasActiveTab">True when a tab is selected in the shell.</param>
/// <param name="ActiveTabDirty">Dirty flag of the active tab.</param>
/// <param name="HasFocusedText">True when a text field has focus (Edit clipboard commands).</param>
/// <param name="FocusTabKey">Set by the launcher's Recent rows: focus the tab with this key.</param>
public sealed record CommandContext(
    WorkspaceTabViewModel? ActiveTab = null,
    bool HasActiveTab = false,
    bool ActiveTabDirty = false,
    bool HasFocusedText = false,
    string? FocusTabKey = null);

/// <summary>Outcome of one dispatch.</summary>
/// <param name="Executed">The handler ran to completion.</param>
/// <param name="Availability">The availability the check answered (reason included).</param>
/// <param name="Duplicate">A second trigger arrived while the command was running and was ignored.</param>
public sealed record DispatchResult(bool Executed, CommandAvailability Availability, bool Duplicate = false);

/// <summary>
/// Ask the availability provider whether a command can run. The shell's local provider
/// implements the chrome-level rules; a later task wires the same seam to the server.
/// </summary>
public delegate CommandAvailability AvailabilityProvider(CommandDefinition definition, CommandContext context);

/// <summary>
/// The single door every trigger passes through: menu click, launcher Enter, keyboard
/// chord and tab context menu. Checks availability, ignores a trigger for a command that
/// is already running, then runs the registered handler.
/// </summary>
public sealed class CommandDispatcher
{
    private readonly CommandRegistry _registry;
    private readonly Dictionary<string, Func<CommandContext, Task>> _handlers = new(StringComparer.Ordinal);
    private readonly HashSet<string> _running = new(StringComparer.Ordinal);
    private readonly object _gate = new();

    public CommandDispatcher(CommandRegistry registry)
    {
        _registry = registry ?? throw new ArgumentNullException(nameof(registry));
    }

    /// <summary>Local availability: the shell answers from state until the server seam lands.</summary>
    public AvailabilityProvider? Availability { get; set; }

    public void Register(string id, Func<CommandContext, Task> handler)
    {
        ArgumentNullException.ThrowIfNull(handler);
        _ = _registry[id];
        _handlers.Add(id, handler);
    }

    /// <summary>True when a handler is registered: a command in the registry with no
    /// handler is a stub, and the shell registers none (brief: absent, not stubbed).</summary>
    public bool HasHandler(string id) => _handlers.ContainsKey(id);

    /// <summary>Current availability of a command under <paramref name="context"/> (menu
    /// grey-out and launcher reasons read this, never a parallel implementation).</summary>
    public CommandAvailability GetAvailability(string id, CommandContext context)
    {
        var definition = _registry[id];
        return Availability?.Invoke(definition, context) ?? CommandAvailability.Available;
    }

    public async Task<DispatchResult> ExecuteAsync(string id, CommandContext context, CommandRoute route)
    {
        _ = _registry[id];
        _ = route; // route is diagnostic only — behaviour must not depend on it
        lock (_gate)
        {
            if (!_running.Add(id))
            {
                return new DispatchResult(false, CommandAvailability.Blocked("Already running."), Duplicate: true);
            }
        }

        try
        {
            var availability = GetAvailability(id, context);
            if (!availability.IsAvailable)
            {
                return new DispatchResult(false, availability);
            }

            if (_handlers.TryGetValue(id, out var handler))
            {
                // No ConfigureAwait(false): chrome commands mutate bound UI state and
                // must resume where they were triggered (the UI thread under Avalonia).
                await handler(context);
            }

            return new DispatchResult(true, availability);
        }
        finally
        {
            lock (_gate)
            {
                _running.Remove(id);
            }
        }
    }
}
