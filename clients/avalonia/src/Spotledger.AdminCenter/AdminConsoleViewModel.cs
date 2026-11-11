using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter;

/// <summary>
/// The console frame's state: who is signed in, what the licence grants, the navigation
/// computed from both, and the 'show equivalent command' pane for the pending action.
/// </summary>
public sealed class AdminConsoleViewModel : ObservableObject
{
    private readonly ToolRegistry _registry;
    private readonly EquivalentCommand _commands;
    private AdminPrincipal _principal;
    private Entitlements _entitlements;
    private AdminAction? _pending;
    private SigningSession? _signing;

    public AdminConsoleViewModel(ToolRegistry registry, EquivalentCommand commands, AdminPrincipal principal, Entitlements entitlements)
    {
        _registry = registry;
        _commands = commands;
        _principal = principal;
        _entitlements = entitlements;
    }

    public IReadOnlyList<NavigationItem> Navigation => _registry.Navigation(_principal, _entitlements);

    public string? EquivalentCommandText => _pending is null ? null : _commands.Cli(_pending);

    public string? SignedPreviewText => _pending is null ? null : _commands.SignedCommandPreview(_pending);

    public AdminAction? Pending
    {
        get => _pending;
        set
        {
            if (SetField(ref _pending, value))
            {
                _signing = value is null ? null : new SigningSession(value, _commands);
                OnPropertyChanged(nameof(Signing));
                OnPropertyChanged(nameof(EquivalentCommandText));
                OnPropertyChanged(nameof(SignedPreviewText));
            }
        }
    }

    /// <summary>The signing flow for the pending action (null when nothing is pending).</summary>
    public SigningSession? Signing => _signing;

    public void SignIn(AdminPrincipal principal, Entitlements entitlements)
    {
        _principal = principal;
        _entitlements = entitlements;
        OnPropertyChanged(nameof(Navigation));
    }

    /// <summary>
    /// Executes the pending action through the same record the pane renders. Fails closed:
    /// an operation that needs signatures does not run until its signing session is complete.
    /// </summary>
    public Task ExecutePendingAsync(IAdminExecutor executor, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(executor);
        if (_pending is null)
        {
            throw new InvalidOperationException("There is no pending action.");
        }

        if (_signing is null || _signing.State is not (SigningState.NotRequired or SigningState.Complete))
        {
            throw new InvalidOperationException("The pending action still needs signatures.");
        }

        return executor.ExecuteAsync(_pending, cancellationToken);
    }
}
