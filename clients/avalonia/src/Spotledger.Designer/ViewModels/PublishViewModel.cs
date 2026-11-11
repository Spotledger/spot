using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>
/// The publish dialog: the server's plan summary (how risky it is and what it changes), the
/// blocking issues, an explicit confirmation, and the result.
/// </summary>
/// <remarks>
/// The dialog is convenience, not the gate. The button is disabled without the publish permission
/// and until the plan is ready, but the server checks the permission and the plan again when the
/// publish is confirmed, and its refusal is what the designer sees.
/// </remarks>
public sealed class PublishViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private bool _isConfirmed;
    private bool _isOpen;
    private string _result = string.Empty;

    /// <summary>Creates the dialog over a session.</summary>
    public PublishViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        ConfirmCommand = new AsyncRelayCommand(_ => ConfirmAsync(), _ => CanConfirm);
    }

    /// <summary>Confirms the publish; enabled only when the plan is ready, permitted and confirmed.</summary>
    public AsyncRelayCommand ConfirmCommand { get; }

    /// <summary>Raised when the dialog should close (after a successful publish).</summary>
    public event EventHandler? Closed;

    /// <summary>Whether the dialog is showing.</summary>
    public bool IsOpen
    {
        get => _isOpen;
        private set => SetField(ref _isOpen, value);
    }

    /// <summary>The plan the server built.</summary>
    public PublishPlanSummary? Plan { get; private set; }

    /// <summary>The changes the publish would apply, as sentences.</summary>
    public ObservableCollection<PublishChange> Changes { get; } = new();

    /// <summary>The issues that block the publish.</summary>
    public ObservableCollection<ValidationIssue> BlockingIssues { get; } = new();

    /// <summary>The risk class in a person's words.</summary>
    public string RiskText => Plan?.RiskText ?? string.Empty;

    /// <summary>Whether this design creates a new DocType.</summary>
    public string SubjectText => Plan is null
        ? string.Empty
        : Plan.NewDoctype
            ? $"Creates the new DocType {Plan.Doctype}."
            : $"Changes the existing DocType {Plan.Doctype}.";

    /// <summary>Whether the designer has to tick the confirmation box.</summary>
    public bool IsConfirmed
    {
        get => _isConfirmed;
        set
        {
            if (SetField(ref _isConfirmed, value))
            {
                OnPropertyChanged(nameof(CanConfirm));
                ConfirmCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>Whether the server says this designer may publish designs at all.</summary>
    public bool CanPublish => _session.Draft?.CanPublish ?? false;

    /// <summary>Why the publish button is disabled, or empty.</summary>
    public string BlockedReason
    {
        get
        {
            if (!CanPublish)
            {
                return "You do not have permission to publish designs. Ask an administrator for the designer publish permission.";
            }

            if (Plan is not null && !Plan.Ready)
            {
                return "The server says this design cannot publish yet. Fix the issues listed below.";
            }

            return IsConfirmed ? string.Empty : "Tick the confirmation box to publish.";
        }
    }

    /// <summary>Whether the publish can be confirmed now.</summary>
    public bool CanConfirm => CanPublish && Plan?.Ready == true && IsConfirmed;

    /// <summary>The result of the last confirmation.</summary>
    public string Result
    {
        get => _result;
        private set => SetField(ref _result, value);
    }

    /// <summary>Builds the plan and opens the dialog.</summary>
    public async Task OpenAsync()
    {
        Plan = await _session.PlanPublishAsync().ConfigureAwait(true);
        Changes.Clear();
        BlockingIssues.Clear();
        foreach (var change in Plan.ChangeList)
        {
            Changes.Add(change);
        }

        foreach (var issue in Plan.IssueList.Where(i => i.Severity == IssueSeverity.Error))
        {
            BlockingIssues.Add(issue);
        }

        IsConfirmed = false;
        Result = string.Empty;
        IsOpen = true;
        OnPropertyChanged(nameof(Plan));
        OnPropertyChanged(nameof(RiskText));
        OnPropertyChanged(nameof(SubjectText));
        OnPropertyChanged(nameof(CanPublish));
        OnPropertyChanged(nameof(BlockedReason));
        OnPropertyChanged(nameof(CanConfirm));
        ConfirmCommand.RaiseCanExecuteChanged();
    }

    /// <summary>Closes the dialog without publishing.</summary>
    public void Close() => IsOpen = false;

    /// <summary>Publishes the plan; the server decides.</summary>
    public async Task ConfirmAsync()
    {
        if (!CanConfirm)
        {
            return;
        }

        var result = await _session.PublishAsync().ConfigureAwait(true);
        Result = result.Ok
            ? $"{_session.LastReceipt?.Doctype} published as version {_session.LastReceipt?.Version}."
            : $"{result.Message}";
        OnPropertyChanged(nameof(Result));
        if (result.Ok)
        {
            IsOpen = false;
            Closed?.Invoke(this, EventArgs.Empty);
        }
    }
}
