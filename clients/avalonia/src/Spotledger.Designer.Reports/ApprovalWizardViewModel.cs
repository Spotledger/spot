using System.Collections.ObjectModel;
using System.Globalization;

namespace Spotledger.Designer.Reports;

/// <summary>The wizard's steps, in order.</summary>
public enum WizardStep
{
    /// <summary>Which kind of document needs approval.</summary>
    Document,

    /// <summary>Who approves.</summary>
    Approver,

    /// <summary>How long they have and who is asked next.</summary>
    TimeLimit,

    /// <summary>What happens on approval and on rejection.</summary>
    Outcomes,

    /// <summary>The read-only flow and the publish button.</summary>
    Review,
}

/// <summary>The unit of the time limit.</summary>
public enum TimeUnit
{
    /// <summary>Hours.</summary>
    Hours,

    /// <summary>Days.</summary>
    Days,
}

/// <summary>One box of the read-only flow diagram.</summary>
public sealed record FlowNode(string Kind, string Text);

/// <summary>
/// The simple approval wizard: choose the document type, who approves, the time limit and what
/// happens when it is exceeded, and what happens on approval and rejection; then review a
/// read-only flow and publish. The wizard's output is the server's <see cref="ApprovalTemplate"/>
/// input exactly; the server turns it into a process and validates it.
/// </summary>
public sealed class ApprovalWizardViewModel : ObservableObject
{
    private readonly IBuilderServer _server;
    private readonly string _app;
    private WizardStep _step = WizardStep.Document;
    private string _docType = string.Empty;
    private string _title = string.Empty;
    private ApproverKind _approverKind = ApproverKind.Role;
    private string _approverName = string.Empty;
    private int _limit = 2;
    private TimeUnit _unit = TimeUnit.Days;
    private ApproverKind _escalationKind = ApproverKind.Role;
    private string _escalationName = string.Empty;
    private string _approveField = string.Empty;
    private string _approveValue = string.Empty;
    private bool _submitOnApprove;
    private string _rejectField = string.Empty;
    private string _rejectValue = string.Empty;
    private StartOn _startOn = StartOn.Created;
    private string _problem = string.Empty;
    private string _result = string.Empty;

    /// <summary>Creates the wizard for the app the workflow will belong to.</summary>
    public ApprovalWizardViewModel(IBuilderServer server, string app)
    {
        _server = server ?? throw new ArgumentNullException(nameof(server));
        _app = app ?? throw new ArgumentNullException(nameof(app));
        NextCommand = new AsyncRelayCommand(_ => NextAsync());
        BackCommand = new RelayCommand(_ => Back(), _ => _step != WizardStep.Document);
        PublishCommand = new AsyncRelayCommand(_ => PublishAsync(), _ => _step == WizardStep.Review && Issues.All(i => !i.IsError));

        // A failure that escapes a command is shown on the wizard, never swallowed.
        NextCommand.ExecutionFailed += exception => Problem = Guard.Describe(exception);
        PublishCommand.ExecutionFailed += exception => Result = Guard.Describe(exception);
    }

    /// <summary>Record types matching the last search, for the document picker.</summary>
    public ObservableCollection<Spotledger.Client.Designer.DocTypeChoice> DocTypeChoices { get; } = new();

    /// <summary>Searches the record types a workflow can be attached to.</summary>
    public async Task SearchDocTypesAsync(string query, CancellationToken cancellationToken = default)
    {
        var found = new List<Spotledger.Client.Designer.DocTypeChoice>();
        var ok = await Guard.RunAsync(async () =>
        {
            found.AddRange(await _server.SearchDocTypesAsync(query ?? string.Empty, cancellationToken).ConfigureAwait(true));
            return true;
        }, message => Problem = message).ConfigureAwait(true);
        if (!ok)
        {
            return;
        }

        DocTypeChoices.Clear();
        foreach (var choice in found)
        {
            DocTypeChoices.Add(choice);
        }
    }

    /// <summary>The person-readable approver kinds, in the order the pickers list them.</summary>
    public static IReadOnlyList<ApproverKind> ApproverKinds { get; } = new[] { ApproverKind.Role, ApproverKind.User, ApproverKind.Field, ApproverKind.ManagerChain };

    /// <summary>Goes to the next step, after checking this one.</summary>
    public AsyncRelayCommand NextCommand { get; }

    /// <summary>Goes back one step.</summary>
    public RelayCommand BackCommand { get; }

    /// <summary>Publishes the approval as a draft process (the Review step).</summary>
    public AsyncRelayCommand PublishCommand { get; }

    /// <summary>The current step.</summary>
    public WizardStep Step
    {
        get => _step;
        private set
        {
            if (SetField(ref _step, value))
            {
                BackCommand.RaiseCanExecuteChanged();
                PublishCommand.RaiseCanExecuteChanged();
                OnPropertyChanged(nameof(IsReview));
            }
        }
    }

    /// <summary>Whether the review step is showing.</summary>
    public bool IsReview => _step == WizardStep.Review;

    /// <summary>The document type that needs approval.</summary>
    public string DocType
    {
        get => _docType;
        set => SetField(ref _docType, value ?? string.Empty);
    }

    /// <summary>The workflow's title; a default is suggested from the document type.</summary>
    public string Title
    {
        get => string.IsNullOrWhiteSpace(_title) && _docType.Length > 0 ? $"{_docType} approval" : _title;
        set => SetField(ref _title, value ?? string.Empty);
    }

    /// <summary>The kind of approver: role, a person, a field of the document, or the manager chain.</summary>
    public ApproverKind ApproverKind
    {
        get => _approverKind;
        set => SetField(ref _approverKind, value);
    }

    /// <summary>The role, person or field name.</summary>
    public string ApproverName
    {
        get => _approverName;
        set => SetField(ref _approverName, value ?? string.Empty);
    }

    /// <summary>The time limit, in <see cref="Unit"/>s.</summary>
    public int Limit
    {
        get => _limit;
        set => SetField(ref _limit, value);
    }

    /// <summary>The time limit's unit.</summary>
    public TimeUnit Unit
    {
        get => _unit;
        set => SetField(ref _unit, value);
    }

    /// <summary>The kind of approver asked when the time limit is exceeded.</summary>
    public ApproverKind EscalationKind
    {
        get => _escalationKind;
        set => SetField(ref _escalationKind, value);
    }

    /// <summary>The role, person or field asked when the time limit is exceeded.</summary>
    public string EscalationName
    {
        get => _escalationName;
        set => SetField(ref _escalationName, value ?? string.Empty);
    }

    /// <summary>The field to set on approval; empty for none.</summary>
    public string ApproveField
    {
        get => _approveField;
        set => SetField(ref _approveField, value ?? string.Empty);
    }

    /// <summary>The value to write on approval.</summary>
    public string ApproveValue
    {
        get => _approveValue;
        set => SetField(ref _approveValue, value ?? string.Empty);
    }

    /// <summary>Whether approval also submits the document.</summary>
    public bool SubmitOnApprove
    {
        get => _submitOnApprove;
        set => SetField(ref _submitOnApprove, value);
    }

    /// <summary>The field to set on rejection; empty for none.</summary>
    public string RejectField
    {
        get => _rejectField;
        set => SetField(ref _rejectField, value ?? string.Empty);
    }

    /// <summary>The value to write on rejection.</summary>
    public string RejectValue
    {
        get => _rejectValue;
        set => SetField(ref _rejectValue, value ?? string.Empty);
    }

    /// <summary>When the approval starts.</summary>
    public StartOn StartOn
    {
        get => _startOn;
        set => SetField(ref _startOn, value);
    }

    /// <summary>What is wrong with the current step, in plain words, or empty.</summary>
    public string Problem
    {
        get => _problem;
        private set => SetField(ref _problem, value);
    }

    /// <summary>What the last publish said.</summary>
    public string Result
    {
        get => _result;
        private set => SetField(ref _result, value);
    }

    /// <summary>The server's validation of the template, shown on the review step.</summary>
    public ObservableCollection<BuilderIssue> Issues { get; } = new();

    /// <summary>The read-only flow diagram: a simple linear diagram of the template.</summary>
    public ObservableCollection<FlowNode> Flow { get; } = new();

    /// <summary>The template the wizard has collected: exactly the server's input.</summary>
    public ApprovalTemplate BuildTemplate()
    {
        var approve = new List<ApprovalAction>();
        if (_approveField.Length > 0)
        {
            approve.Add(new ApprovalAction.SetField(_approveField, _approveValue));
        }

        if (_submitOnApprove)
        {
            approve.Add(new ApprovalAction.Submit());
        }

        var reject = new List<ApprovalAction>();
        if (_rejectField.Length > 0)
        {
            reject.Add(new ApprovalAction.SetField(_rejectField, _rejectValue));
        }

        var seconds = (long)_limit * (_unit == TimeUnit.Days ? 86_400L : 3_600L);
        return new ApprovalTemplate(
            Name: ProcessName(_docType),
            Title: Title,
            App: _app,
            DocType: _docType,
            Approvers: new Approver(_approverKind, _approverName),
            SlaSeconds: seconds,
            EscalateTo: new Approver(_escalationKind, _escalationName),
            OnApprove: approve,
            OnReject: reject,
            StartOn: _startOn);
    }

    /// <summary>The process name suggested for a document type, in the site's name style.</summary>
    public static string ProcessName(string docType)
    {
        var text = BuilderJson.ToSnake((docType ?? string.Empty).Trim()).Replace(' ', '_').Replace('-', '_');
        while (text.Contains("__", StringComparison.Ordinal))
        {
            text = text.Replace("__", "_", StringComparison.Ordinal);
        }

        return text.Length == 0 ? string.Empty : $"approve_{text.Trim('_')}";
    }

    /// <summary>Checks the current step in plain words; the server checks the whole template at review.</summary>
    public string? CheckStep(WizardStep step) => step switch
    {
        WizardStep.Document => _docType.Trim().Length == 0 ? "Choose the document that needs approval." : null,
        WizardStep.Approver => _approverName.Trim().Length == 0 ? "Say who approves: a role, a person, or a field that holds the approver." : null,
        WizardStep.TimeLimit => _limit < 1
            ? "The time limit must be at least 1."
            : _escalationName.Trim().Length == 0 ? "Say who is asked when the time limit is exceeded." : null,
        WizardStep.Outcomes => _approveField.Length > 0 && _approveValue.Length == 0
            ? "Say what value to write on approval."
            : _rejectField.Length > 0 && _rejectValue.Length == 0 ? "Say what value to write on rejection." : null,
        _ => null,
    };

    private async Task NextAsync()
    {
        if (_step == WizardStep.Review)
        {
            return;
        }

        var problem = CheckStep(_step);
        Problem = problem ?? string.Empty;
        if (problem is not null)
        {
            return;
        }

        var next = _step + 1;
        if (next == WizardStep.Review)
        {
            await ReviewAsync().ConfigureAwait(true);
        }

        Step = next;
    }

    private void Back()
    {
        Problem = string.Empty;
        if (_step != WizardStep.Document)
        {
            Step = _step - 1;
        }
    }

    private async Task ReviewAsync()
    {
        var template = BuildTemplate();
        BuildFlow(template);
        var issues = await _server.ValidateApprovalAsync(template).ConfigureAwait(true);
        Issues.Clear();
        foreach (var issue in issues)
        {
            Issues.Add(issue);
        }
    }

    private async Task PublishAsync()
    {
        var outcome = await _server.PublishApprovalAsync(BuildTemplate()).ConfigureAwait(true);
        PublishCommand.RaiseCanExecuteChanged();
        Result = outcome.Message;
        if (!outcome.Ok)
        {
            Issues.Clear();
            foreach (var issue in outcome.Issues)
            {
                Issues.Add(issue);
            }
        }
    }

    private void BuildFlow(ApprovalTemplate template)
    {
        Flow.Clear();
        Flow.Add(new FlowNode("start", template.StartOn == StartOn.Created ? $"A {template.DocType} is created" : $"Someone starts the {template.DocType} approval"));
        Flow.Add(new FlowNode("approval", $"{Describe(template.Approvers)} approves, within {DescribeLimit(template.SlaSeconds)}"));
        Flow.Add(new FlowNode("escalation", $"If it takes longer: {Describe(template.EscalateTo)} is asked instead"));
        Flow.Add(new FlowNode("approved", "Approved" + Actions(template.OnApprove)));
        Flow.Add(new FlowNode("rejected", "Rejected" + Actions(template.OnReject)));
    }

    private static string Actions(IReadOnlyList<ApprovalAction> actions) =>
        actions.Count == 0
            ? string.Empty
            : ": " + string.Join(", then ", actions.Select(a => a switch
            {
                ApprovalAction.SetField s => $"set {s.Field} to {s.Value}",
                ApprovalAction.Submit => "submit the document",
                _ => string.Empty,
            }));

    private static string Describe(Approver approver) => approver.Kind switch
    {
        ApproverKind.Role => $"Anyone with the role {approver.Name}",
        ApproverKind.User => approver.Name,
        ApproverKind.Field => $"The person in {approver.Name}",
        ApproverKind.ManagerChain => $"The manager of {approver.Name}, then their manager",
        _ => approver.Name,
    };

    private static string DescribeLimit(long seconds)
    {
        if (seconds % 86_400 == 0)
        {
            var days = seconds / 86_400;
            return string.Create(CultureInfo.InvariantCulture, $"{days} {(days == 1 ? "day" : "days")}");
        }

        var hours = seconds / 3_600;
        return string.Create(CultureInfo.InvariantCulture, $"{hours} {(hours == 1 ? "hour" : "hours")}");
    }
}
