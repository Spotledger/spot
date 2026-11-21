using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>
/// The validation panel: the server's live report for the draft, in plain language, with the
/// element each issue is about so a click selects it on the canvas or in the inspector.
/// </summary>
public sealed class ValidationPanelViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private int _errors;
    private int _warnings;

    /// <summary>Creates the panel over a session.</summary>
    public ValidationPanelViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        SelectIssueCommand = new RelayCommand(p => Select(p as ValidationIssue));
        RefreshCommand = new AsyncRelayCommand(_ => RefreshAsync());
    }

    /// <summary>Raised when an issue's element should be shown: the canvas and inspector listen.</summary>
    public event EventHandler<ValidationIssue>? NavigateRequested;

    /// <summary>The issues the server last reported, in its order.</summary>
    public ObservableCollection<ValidationIssue> Issues { get; } = new();

    /// <summary>Selects the element an issue is about.</summary>
    public RelayCommand SelectIssueCommand { get; }

    /// <summary>Asks the server to validate again.</summary>
    public AsyncRelayCommand RefreshCommand { get; }

    /// <summary>Whether the design can publish as far as validation is concerned.</summary>
    public bool IsOk => _errors == 0;

    /// <summary>"No issues" or the counts, for the panel's header.</summary>
    public string Summary => (_errors, _warnings) switch
    {
        (0, 0) => "No issues",
        (0, _) => $"{_warnings} to check",
        (_, 0) => $"{_errors} to fix",
        _ => $"{_errors} to fix, {_warnings} to check",
    };

    /// <summary>How many issues must be fixed before publishing.</summary>
    public int ErrorCount => _errors;

    /// <summary>How many issues are worth checking.</summary>
    public int WarningCount => _warnings;

    /// <summary>Reloads the panel from the server's last report.</summary>
    public void Rebuild()
    {
        Issues.Clear();
        var report = _session.Validation;
        if (report is not null)
        {
            foreach (var issue in report.IssueList)
            {
                Issues.Add(issue);
            }
        }

        _errors = Issues.Count(i => i.Severity == IssueSeverity.Error);
        _warnings = Issues.Count - _errors;
        OnPropertyChanged(nameof(IsOk));
        OnPropertyChanged(nameof(Summary));
        OnPropertyChanged(nameof(ErrorCount));
        OnPropertyChanged(nameof(WarningCount));
    }

    /// <summary>Validates again on the server and reloads the panel.</summary>
    public Task RefreshAsync() =>
        _session.RefreshAsync().ContinueWith(
            _ => Rebuild(),
            CancellationToken.None,
            TaskContinuationOptions.ExecuteSynchronously,
            TaskScheduler.Default);

    /// <summary>Selects an issue's element and asks the view to show it.</summary>
    public void Select(ValidationIssue? issue)
    {
        if (issue is null)
        {
            return;
        }

        _session.Select(issue);
        NavigateRequested?.Invoke(this, issue);
    }
}
