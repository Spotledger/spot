using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// One row of the run list: the words a person reads and the run they open.
public sealed record RunListEntry(string Text, RunSummary Run);

/// The whole test-run workspace: the canvas with its badges, the sample-data box with the
/// trigger's hints, the four step-through controls, the node state list and the history panel
/// (run list, node timeline, and the input and output panes of the selected attempt).
///
/// Every action is reachable from the keyboard alone — the buttons carry their shortcut in the
/// accessible name and <see cref="HandleKeyAsync"/> runs the same command — so nothing here needs a
/// pointer.
public sealed class RunWorkspace : UserControl
{

    private readonly FlowRunSession _session;

    private readonly RunCanvas _canvas = new();

    private readonly TextBox _sampleData = new();

    private readonly ListBox _hints = new();

    private readonly ListBox _states = new();

    private readonly ListBox _runs = new();

    private readonly ListBox _timeline = new();

    private readonly ListBox _input = new();

    private readonly ListBox _output = new();

    private readonly TextBlock _status = new();

    private readonly TextBlock _sampleHeading = new();

    private readonly TextBlock _hintHeading = new();

    private readonly TextBlock _stateHeading = new();

    private readonly TextBlock _historyHeading = new();

    private readonly TextBlock _timelineHeading = new();

    private readonly TextBlock _inputHeading = new();

    private readonly TextBlock _outputHeading = new();

    private readonly Button _runTest;

    private readonly Button _continue;

    private readonly Button _step;

    private readonly Button _stop;

    public RunWorkspace(FlowRunSession session)
    {
        ArgumentNullException.ThrowIfNull(session);
        _session = session;
        _canvas.Session = session;

        _sampleHeading.Text = RunStrings.SampleDataName;
        _hintHeading.Text = RunStrings.SampleDataHintName;
        _stateHeading.Text = RunStrings.StatusName;
        _historyHeading.Text = RunStrings.HistoryHeading;
        _timelineHeading.Text = RunStrings.TimelineHeading;
        _inputHeading.Text = RunStrings.InputPane;
        _outputHeading.Text = RunStrings.OutputPane;

        _sampleData.AcceptsReturn = true;
        _sampleData.TextWrapping = Avalonia.Media.TextWrapping.Wrap;
        _sampleData.Text = session.SampleData.Text;
        _sampleData.Height = 120;
        _sampleData.PlaceholderText = RunStrings.SampleDataHelp;
        AutomationPropertiesShim(_sampleData, "run-sample-data", RunStrings.SampleDataName, RunStrings.SampleDataHelp);
        _sampleData.TextChanged += (_, _) => OnSampleDataChanged();

        _runTest = Command(RunStrings.RunTest, "run-test", RunStrings.Shortcut(RunStrings.RunTest, "F5"), StartTestAsync);
        _continue = Command(RunStrings.Continue, "run-continue", RunStrings.Shortcut(RunStrings.Continue, "F8"), ContinueAsync);
        _step = Command(RunStrings.Step, "run-step", RunStrings.Shortcut(RunStrings.Step, "F10"), StepAsync);
        _stop = Command(RunStrings.Stop, "run-stop", RunStrings.Shortcut(RunStrings.Stop, "F9"), StopAsync);

        _runs.SelectionChanged += (_, _) => _ = OpenSelectedAsync();
        _timeline.SelectionChanged += (_, _) => ShowSelectedExchange();

        _status.Text = RunStrings.SampleDataHelp;
        _status.TextWrapping = Avalonia.Media.TextWrapping.Wrap;

        Content = BuildLayout();
        _session.Changed += (_, _) => Refresh();
        Refresh();
    }

    /// The session this workspace drives.
    public FlowRunSession Session => _session;

    /// The canvas the badges are painted on.
    public RunCanvas Canvas => _canvas;

    public TextBox SampleDataBox => _sampleData;

    public ListBox HintList => _hints;

    public ListBox StateList => _states;

    public ListBox RunList => _runs;

    public ListBox TimelineList => _timeline;

    public ListBox InputPaneList => _input;

    public ListBox OutputPaneList => _output;

    public TextBlock StatusLine => _status;

    public Button RunTestButton => _runTest;

    public Button ContinueButton => _continue;

    public Button StepButton => _step;

    public Button StopButton => _stop;

    /// The history as its saved text form, which is what "save the history" writes.
    public string HistoryText() => _session.History.ToText();

    /// Runs the command a shortcut stands for. F5 runs a test, F8 continues, F10 steps one node and
    /// F9 stops; anything else is not this workspace's key. Answers whether it was handled.
    public async Task<bool> HandleKeyAsync(Key key, CancellationToken cancellationToken)
    {
        switch (key)
        {
            case Key.F5:
                await StartTestAsync().ConfigureAwait(true);
                return true;
            case Key.F8:
                await ContinueAsync().ConfigureAwait(true);
                return true;
            case Key.F10:
                await StepAsync().ConfigureAwait(true);
                return true;
            case Key.F9:
                await StopAsync().ConfigureAwait(true);
                return true;
            default:
                return false;
        }
    }

    /// Rebuilds every list and every enabled state from the session. Called on each change the
    /// session reports, so the workspace never holds a stale view.
    public void Refresh()
    {
        _status.Text = _session.ErrorMessage ?? _session.StatusMessage ?? _session.SampleData.Summary();

        _hints.ItemsSource = HintLines();

        List<string> states = [];
        foreach (RunNodeBadge badge in _session.Badges)
        {
            states.Add(badge.HelpText);
        }

        if (states.Count == 0)
        {
            states.Add(RunStrings.NoRunYet);
        }

        _states.ItemsSource = states;

        List<RunListEntry> entries = [];
        foreach (RunHistoryGroup group in _session.History.Groups)
        {
            foreach (RunSummary run in group.Runs)
            {
                entries.Add(new RunListEntry(group.Heading + " · " + _session.Strings.RunHeading(run), run));
            }
        }

        if (entries.Count == 0)
        {
            entries.Add(new RunListEntry(_session.Strings.NoRuns, EmptyRun()));
        }

        _runs.ItemsSource = entries;
        _historyHeading.Text = RunStrings.HistoryHeading;
        _timelineHeading.Text = _session.History.Heading;

        List<string> rows = [];
        foreach (RunTimelineRow row in _session.History.Timeline)
        {
            rows.Add(row.Text);
        }

        if (rows.Count == 0)
        {
            rows.Add(_session.Strings.NoTimeline);
        }

        _timeline.ItemsSource = rows;
        ShowSelectedExchange();

        _runTest.IsEnabled = !_session.IsBusy;
        _continue.IsEnabled = _session.CanContinue && !_session.IsBusy;
        _step.IsEnabled = _session.CanStep && !_session.IsBusy;
        _stop.IsEnabled = _session.CanStop && !_session.IsBusy;
    }

    private void ShowSelectedExchange()
    {
        int index = _timeline.SelectedIndex;
        RunExchange? exchange = null;
        if (index >= 0 && index < _session.History.Exchanges.Count)
        {
            exchange = _session.History.Exchanges[index];
        }

        IReadOnlyList<string> inputLines = exchange is null ? [RunStrings.NoInput] : exchange.InputLines;
        IReadOnlyList<string> outputLines = exchange is null ? [RunStrings.NoOutput] : exchange.OutputLines;
        _input.ItemsSource = new List<string>(inputLines);
        _output.ItemsSource = new List<string>(outputLines);
        _inputHeading.Text = exchange is null
            ? RunStrings.InputPane
            : RunStrings.InputPane + " · " + exchange.Title;
        _outputHeading.Text = exchange is null
            ? RunStrings.OutputPane
            : RunStrings.OutputPane + " · " + exchange.Title;
    }

    private async Task OpenSelectedAsync()
    {
        if (_runs.SelectedItem is not RunListEntry entry || string.IsNullOrEmpty(entry.Run.RunId))
        {
            return;
        }

        await _session.OpenRunAsync(entry.Run.RunId, CancellationToken.None).ConfigureAwait(true);
    }

    private async Task StartTestAsync() => await _session.StartTestAsync(CancellationToken.None).ConfigureAwait(true);

    private async Task ContinueAsync() => await _session.ContinueAsync(CancellationToken.None).ConfigureAwait(true);

    private async Task StepAsync() => await _session.StepAsync(CancellationToken.None).ConfigureAwait(true);

    private async Task StopAsync() => await _session.StopAsync(CancellationToken.None).ConfigureAwait(true);

    private void OnSampleDataChanged()
    {
        if (string.Equals(_session.SampleData.Text, _sampleData.Text, StringComparison.Ordinal))
        {
            return;
        }

        _session.SampleData.Text = _sampleData.Text ?? string.Empty;
        Refresh();
    }

    private static RunSummary EmptyRun() =>
        new(string.Empty, string.Empty, 0, RunOutcome.Running, DateTimeOffset.UnixEpoch);

    private List<string> HintLines()
    {
        List<string> lines = [];
        foreach (SampleDataHint hint in _session.SampleData.Hints)
        {
            lines.Add(hint.Key + ": " + hint.Text);
        }

        return lines;
    }

    private static Button Command(string text, string automationId, string help, Func<Task> action)
    {
        Button button = new() { Content = text, Focusable = true };
        AutomationPropertiesShim(button, automationId, text, help);
        button.Click += (_, _) => _ = Run(action);
        return button;
    }

    private static async Task Run(Func<Task> action)
    {
        try
        {
            await action().ConfigureAwait(true);
        }
        catch (RunRefusedException)
        {
            // The session already turned the refusal into a message; a click handler has nowhere to
            // put an exception, and the workspace shows the reason on its status line.
        }
    }

    private static void AutomationPropertiesShim(Control control, string automationId, string name, string help)
    {
        Avalonia.Automation.AutomationProperties.SetAutomationId(control, automationId);
        Avalonia.Automation.AutomationProperties.SetName(control, name);
        Avalonia.Automation.AutomationProperties.SetHelpText(control, help);
    }

    private Grid BuildLayout()
    {
        StackPanel toolbar = new()
        {
            Orientation = Orientation.Horizontal,
            Spacing = 8,
            Children = { _runTest, _continue, _step, _stop },
        };

        StackPanel sample = new()
        {
            Orientation = Orientation.Vertical,
            Spacing = 4,
            Children = { _sampleHeading, _sampleData, _hintHeading, _hints, _stateHeading, _states },
        };

        Grid body = new()
        {
            ColumnDefinitions = new ColumnDefinitions("2*,*"),
            RowDefinitions = new RowDefinitions("*"),
        };
        body.Children.Add(_canvas);
        Grid.SetColumn(_canvas, 0);
        body.Children.Add(sample);
        Grid.SetColumn(sample, 1);

        StackPanel exchange = new()
        {
            Orientation = Orientation.Vertical,
            Spacing = 2,
            Children = { _inputHeading, _input, _outputHeading, _output },
        };

        StackPanel historyLeft = new()
        {
            Orientation = Orientation.Vertical,
            Spacing = 2,
            Children = { _historyHeading, _runs, _timelineHeading, _timeline },
        };

        Grid history = new()
        {
            ColumnDefinitions = new ColumnDefinitions("2*,*"),
            RowDefinitions = new RowDefinitions("*"),
            Height = 220,
        };
        history.Children.Add(historyLeft);
        Grid.SetColumn(historyLeft, 0);
        history.Children.Add(exchange);
        Grid.SetColumn(exchange, 1);

        Grid root = new()
        {
            RowDefinitions = new RowDefinitions("Auto,*,Auto,Auto"),
        };
        root.Children.Add(toolbar);
        Grid.SetRow(toolbar, 0);
        root.Children.Add(body);
        Grid.SetRow(body, 1);
        root.Children.Add(history);
        Grid.SetRow(history, 2);
        root.Children.Add(_status);
        Grid.SetRow(_status, 3);
        return root;
    }
}
