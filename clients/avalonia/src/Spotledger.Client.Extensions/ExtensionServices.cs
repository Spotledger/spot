namespace Spotledger.Client.Extensions;

/// <summary>What a slot view needs from the shell and the network. A test passes fakes;
/// the desktop head passes the real dispatcher, the shell's services and a query runner
/// that issues the declared query through the native API.</summary>
/// <param name="Actions">Dispatches action requests; null when the host has none (a click reports it).</param>
/// <param name="Shell">Applies a dispatcher's answer (confirm, message, refresh, open, download); null when the host has none.</param>
/// <param name="Queries">Runs a panel's or tile's declared queries; null when the host has none (data blocks show the error state with retry).</param>
public sealed record ExtensionServices(
    IActionDispatcher? Actions = null,
    IShellServices? Shell = null,
    IQueryRunner? Queries = null)
{
    /// <summary>No services: models render, but a dispatch or a data load reports that the host wired nothing.</summary>
    public static ExtensionServices None { get; } = new();
}

/// <summary>One rendered item of a slot. Its visibility is re-evaluated whenever the
/// context changes, and it raises <see cref="Changed" /> so a host can repaint.</summary>
public abstract class SlotEntry
{
    private bool _isVisible = true;

    /// <summary>Creates the entry.</summary>
    protected SlotEntry(string id, string app, int order)
    {
        ArgumentNullException.ThrowIfNull(id);
        ArgumentNullException.ThrowIfNull(app);
        Id = id;
        App = app;
        Order = order;
    }

    /// <summary>The namespaced id.</summary>
    public string Id { get; }

    /// <summary>The contributing app.</summary>
    public string App { get; }

    /// <summary>The ordering hint.</summary>
    public int Order { get; }

    /// <summary>True when the item's permission and <c>visible_if</c> allow it to be shown.</summary>
    public bool IsVisible
    {
        get => _isVisible;
        private protected set
        {
            if (_isVisible == value)
            {
                return;
            }
            _isVisible = value;
            RaiseChanged();
        }
    }

    /// <summary>Raised when the entry's state changes (visibility, busy state, data).</summary>
    public event EventHandler? Changed;

    /// <summary>Raises <see cref="Changed" /> for a subclass.</summary>
    protected void RaiseChanged() => Changed?.Invoke(this, EventArgs.Empty);
}
