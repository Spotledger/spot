using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The whole two-organisation workspace: the canvas with its lanes, badges and regions, the
/// contract editor's panels (roles, messages with their fields, the order table with its generated
/// state diagram, outcomes, wait limits, versions, changes and signatures), the problem list, the
/// partner simulator with its history, and the effect lists. Every panel is a plain list of the
/// words <see cref="ContractRows"/> and the session produce, rebuilt on each change the session
/// reports, so the workspace holds no state of its own and a screen reader reads what the screen
/// shows. Every action is a button with an accessible name; none needs a pointer.
public sealed class ParticipantWorkspace : UserControl
{

    private readonly ParticipantSession _session;

    private readonly ParticipantCanvas _canvas = new();

    private readonly StateDiagramView _diagram = new();

    private readonly ListBox _roles = new();

    private readonly ListBox _messages = new();

    private readonly ListBox _order = new();

    private readonly ListBox _outcomes = new();

    private readonly ListBox _timeouts = new();

    private readonly ListBox _versions = new();

    private readonly ListBox _changes = new();

    private readonly ListBox _signatures = new();

    private readonly ListBox _problems = new();

    private readonly ListBox _steps = new();

    private readonly ListBox _history = new();

    private readonly ListBox _effects = new();

    private readonly TextBox _file = new();

    private readonly TextBox _payload = new();

    private readonly TextBlock _status = new();

    private readonly List<Button> _failureButtons = [];

    public ParticipantWorkspace(ParticipantSession session)
    {
        ArgumentNullException.ThrowIfNull(session);
        _session = session;
        _canvas.Document = session.Document;
        _canvas.Editor = session.Editor;
        _canvas.Catalogue = session.Catalogue;
        _canvas.Session = session;

        _file.AcceptsReturn = true;
        _file.Height = 120;
        Label(_file, "contract-file", ParticipantStrings.ContractFileHeading, ParticipantStrings.ContractFileHelp);
        _payload.AcceptsReturn = true;
        _payload.Height = 80;
        Label(_payload, "simulator-payload", ParticipantStrings.PayloadHeading, ParticipantStrings.PayloadHeading);
        _steps.SelectionChanged += (_, _) => ShowSelectedPayload();
        _status.TextWrapping = TextWrapping.Wrap;

        Content = BuildLayout();
        _session.Changed += (_, _) => Refresh();
        _session.Contract.Changed += (_, _) => Refresh();
        Refresh();
    }

    public ParticipantSession Session => _session;

    public ParticipantCanvas Canvas => _canvas;

    public StateDiagramView Diagram => _diagram;

    public ListBox RoleList => _roles;

    public ListBox MessageList => _messages;

    public ListBox OrderList => _order;

    public ListBox OutcomeList => _outcomes;

    public ListBox TimeoutList => _timeouts;

    public ListBox VersionList => _versions;

    public ListBox ChangeList => _changes;

    public ListBox SignatureList => _signatures;

    public ListBox ProblemList => _problems;

    public ListBox StepList => _steps;

    public ListBox HistoryList => _history;

    public ListBox EffectList => _effects;

    public TextBox ContractFileBox => _file;

    public TextBox PayloadBox => _payload;

    public TextBlock StatusLine => _status;

    /// Fills the file box with the contract a person shares with a partner.
    public void Export() => _file.Text = _session.Contract.Export();

    /// Reads the file box as a contract a partner sent. A file that does not read changes nothing
    /// and the reason is shown on the status line.
    public ContractEditResult Import()
    {
        string text = _file.Text ?? string.Empty;
        ContractEditResult result = text.Trim().Length == 0
            ? ContractEditResult.Refused(ParticipantStrings.NoContractFile)
            : _session.Contract.Import(text);
        if (!result.Ok)
        {
            _status.Text = result.Error;
        }

        return result;
    }

    /// Applies the payload box to the selected simulated message.
    public bool ApplyPayload()
    {
        if (_steps.SelectedIndex < 0 || _steps.SelectedIndex >= _session.Simulator.Steps.Count)
        {
            return false;
        }

        string message = _session.Simulator.Steps[_steps.SelectedIndex].Message.Name;
        bool ok = _session.Simulator.SetPayload(message, _payload.Text ?? string.Empty, out IReadOnlyList<string> problems);
        _status.Text = ok ? ParticipantStrings.SimulatorHeading : string.Join(" ", problems);
        Refresh();
        return ok;
    }

    /// Sets the failure of the selected simulated message.
    public bool SetFailure(SimulatedFailure failure)
    {
        if (_steps.SelectedIndex < 0 || _steps.SelectedIndex >= _session.Simulator.Steps.Count)
        {
            return false;
        }

        int selected = _steps.SelectedIndex;
        bool ok = _session.Simulator.SetFailure(_session.Simulator.Steps[selected].Message.Name, failure);
        Refresh();
        _steps.SelectedIndex = selected;
        return ok;
    }

    /// Rebuilds every panel from the session.
    public void Refresh()
    {
        ProcessContract contract = _session.Contract.Current;
        _status.Text = _session.ErrorMessage ?? _session.StatusMessage ?? _status.Text;
        _roles.ItemsSource = ContractRows.Participants(contract, _session.OwnRole);
        _messages.ItemsSource = ContractRows.Messages(contract, _session.Contract.Classifier);
        _order.ItemsSource = ContractRows.Order(contract);
        _outcomes.ItemsSource = ContractRows.Outcomes(contract);
        _timeouts.ItemsSource = ContractRows.Timeouts(contract);
        _versions.ItemsSource = ContractRows.Versions(_session.Versions);
        _changes.ItemsSource = ContractRows.Changes(_session.Changes);
        _signatures.ItemsSource = ContractRows.Signatures(_session.SignatureRows);
        _diagram.Diagram = _session.Diagram;

        List<string> problems = [.. ContractRows.Problems(_session.ContractProblems)];
        problems.AddRange(FlowProblemList.Rows(_canvas.AllProblems).Select(row => row.AutomationName));
        _problems.ItemsSource = problems;

        int selected = _steps.SelectedIndex;
        _steps.ItemsSource = ContractRows.Simulator(_session.Simulator);
        _steps.SelectedIndex = Math.Min(selected, _session.Simulator.Steps.Count - 1);
        _history.ItemsSource = _session.Simulation?.Rows.Select(row => row.Text).ToList() ?? [ParticipantStrings.NothingYet];

        List<string> effects = [.. _session.Effects.All.Select(badge => badge.HelpText)];
        if (_session.Preview is { } preview)
        {
            effects.Add(preview.Heading);
            effects.AddRange(preview.Groups.SelectMany(group => group.Lines.Select(line => group.NodeId + ": " + line.Text)));
            effects.AddRange(preview.Totals);
        }

        _effects.ItemsSource = effects.Count == 0 ? [ParticipantStrings.NothingYet] : effects;
        ShowSelectedPayload();
    }

    private void ShowSelectedPayload()
    {
        int index = _steps.SelectedIndex;
        _payload.Text = index >= 0 && index < _session.Simulator.Steps.Count ? _session.Simulator.Steps[index].PayloadJson : string.Empty;
    }

    private Grid BuildLayout()
    {
        StackPanel contractButtons = Row(
            Command(ParticipantStrings.CheckContract, "check-contract", () => _session.CheckContractAsync(CancellationToken.None)),
            Command(ParticipantStrings.CheckHalf, "check-half", () => _session.CheckConformanceAsync(CancellationToken.None)),
            Command(ParticipantStrings.ShowChanges, "show-changes", () => _session.LoadChangesAsync(CancellationToken.None)),
            Command(ParticipantStrings.VerifySignatures, "verify-signatures", () => _session.VerifySignaturesAsync(CancellationToken.None)),
            Command(ParticipantStrings.SaveContract, "save-contract", () => _session.SaveContractAsync(CancellationToken.None)));
        Button export = Plain(ParticipantStrings.Export, "export-contract", Export);
        Button import = Plain(ParticipantStrings.Import, "import-contract", () => Import());
        StackPanel fileButtons = Row(export, import);

        foreach (SimulatedFailure failure in Enum.GetValues<SimulatedFailure>())
        {
            Button button = Plain(ParticipantStrings.FailureWord(failure), "failure-" + failure, () => SetFailure(failure));
            _failureButtons.Add(button);
        }

        StackPanel simulatorButtons = Row(
            [
                .. _failureButtons,
                Plain(ParticipantStrings.ApplyPayload, "apply-payload", () => ApplyPayload()),
                Plain(ParticipantStrings.UseCanned, "use-canned", UseCanned),
                Command(ParticipantStrings.RunSimulation, "run-simulation", () => _session.RunSimulationAsync(CancellationToken.None)),
            ]);
        StackPanel effectButtons = Row(
            Command(ParticipantStrings.LoadEffects, "load-effects", () => _session.LoadEffectsAsync(CancellationToken.None)));

        StackPanel side = new()
        {
            Orientation = Orientation.Vertical,
            Spacing = 4,
            Children =
            {
                Heading(ParticipantStrings.RolesHeading), List(_roles, "roles", ParticipantStrings.RolesHeading),
                Heading(ParticipantStrings.MessagesHeading), List(_messages, "messages", ParticipantStrings.MessagesHeading),
                Heading(ParticipantStrings.OrderHeading), List(_order, "order", ParticipantStrings.OrderHeading),
                Heading(ParticipantStrings.DiagramHeading), _diagram,
                Heading(ParticipantStrings.OutcomesHeading), List(_outcomes, "outcomes", ParticipantStrings.OutcomesHeading),
                Heading(ParticipantStrings.TimeoutsHeading), List(_timeouts, "timeouts", ParticipantStrings.TimeoutsHeading),
                Heading(ParticipantStrings.VersionsHeading), List(_versions, "versions", ParticipantStrings.VersionsHeading),
                Heading(ParticipantStrings.ChangesHeading), List(_changes, "changes", ParticipantStrings.ChangesHeading),
                Heading(ParticipantStrings.SignaturesHeading), List(_signatures, "signatures", ParticipantStrings.SignaturesHeading),
                contractButtons,
                Heading(ParticipantStrings.ContractFileHeading), _file, fileButtons,
                Heading(ParticipantStrings.ProblemsHeading), List(_problems, "problems", ParticipantStrings.ProblemsHeading),
                Heading(ParticipantStrings.SimulatorHeading), List(_steps, "simulator-steps", ParticipantStrings.SimulatorHeading),
                Heading(ParticipantStrings.PayloadHeading), _payload, simulatorButtons,
                Heading(ParticipantStrings.SimulationHeadingLabel), List(_history, "simulation-history", ParticipantStrings.SimulationHeadingLabel),
                Heading(ParticipantStrings.EffectsHeading), List(_effects, "effects", ParticipantStrings.EffectsHeading), effectButtons,
                _status,
            },
        };

        Grid body = new()
        {
            ColumnDefinitions = new ColumnDefinitions("2*,*"),
            RowDefinitions = new RowDefinitions("*"),
        };
        body.Children.Add(_canvas);
        Grid.SetColumn(_canvas, 0);
        ScrollViewer scroll = new() { Content = side };
        body.Children.Add(scroll);
        Grid.SetColumn(scroll, 1);
        return body;
    }

    private void UseCanned()
    {
        if (_steps.SelectedIndex >= 0 && _steps.SelectedIndex < _session.Simulator.Steps.Count)
        {
            _session.Simulator.UseCanned(_session.Simulator.Steps[_steps.SelectedIndex].Message.Name);
            Refresh();
        }
    }

    private static TextBlock Heading(string text)
    {
        TextBlock block = new() { Text = text, FontWeight = FontWeight.SemiBold };
        return block;
    }

    private static ListBox List(ListBox list, string id, string name)
    {
        list.Height = 96;
        Label(list, id, name, name);
        return list;
    }

    private static StackPanel Row(params Button[] buttons)
    {
        StackPanel row = new() { Orientation = Orientation.Horizontal, Spacing = 6 };
        row.Children.AddRange(buttons);
        return row;
    }

    private static Button Plain(string text, string id, Action action)
    {
        Button button = new() { Content = text, Focusable = true };
        Label(button, id, text, text);
        button.Click += (_, _) => action();
        return button;
    }

    private Button Command<T>(string text, string id, Func<Task<T>> action)
    {
        Button button = new() { Content = text, Focusable = true };
        Label(button, id, text, text);
        button.Click += (_, _) => _ = Run(async () => await action().ConfigureAwait(true));
        return button;
    }

    private async Task Run(Func<Task> action)
    {
        try
        {
            await action().ConfigureAwait(true);
        }
        catch (SimulationRefusedException refused)
        {
            _status.Text = refused.Message;
        }
        catch (OperationCanceledException)
        {
            // A cancelled request leaves the panels as they were.
        }
    }

    private static void Label(Avalonia.Controls.Control control, string id, string name, string help)
    {
        AutomationProperties.SetAutomationId(control, id);
        AutomationProperties.SetName(control, name);
        AutomationProperties.SetHelpText(control, help);
    }
}
