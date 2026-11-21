namespace Spotledger.AdminCenter.Deploy;

/// <summary>Where a deployment run stands.</summary>
public enum RunState
{
    /// <summary>Not started.</summary>
    NotStarted,

    /// <summary>Steps are running.</summary>
    Running,

    /// <summary>A step failed; the run is stopped there and can be retried.</summary>
    Failed,

    /// <summary>The journal shows a run that was left part-way (the app was closed); it can be continued.</summary>
    Interrupted,

    /// <summary>Every step finished.</summary>
    Complete,
}

/// <summary>One row of the execution view: a step, its status, its log and, when it failed, why.</summary>
public sealed record StepRow(
    int Index,
    string Title,
    string Host,
    StepStatus Status,
    string StatusText,
    IReadOnlyList<string> Log,
    string? FailureReason,
    bool NeedsCleanupBeforeRetry);

/// <summary>
/// The execution view: per-step status and logs from the engine's journal, a progress figure in
/// basis points (no floating point), the time remaining, and retry from the failed step. The same
/// call that started the run resumes it, so it works after a failure and after the app was closed.
/// </summary>
public sealed class RunViewModel : DeployViewModel
{
    private readonly PlanView _plan;
    private readonly IReadOnlyList<PlanSignature> _signatures;
    private IReadOnlyList<StepRow> _rows;
    private RunState _state = RunState.NotStarted;
    private ulong _remainingSeconds;
    private int _selectedIndex = -1;

    /// <summary>Creates the view over a plan and the signatures collected for it (empty when resuming).</summary>
    public RunViewModel(IDeploymentEngine engine, PlanView plan, IReadOnlyList<PlanSignature> signatures)
        : base(engine)
    {
        _plan = plan ?? throw new ArgumentNullException(nameof(plan));
        _signatures = signatures ?? throw new ArgumentNullException(nameof(signatures));
        _remainingSeconds = plan.EstimatedTotalSeconds;
        _rows = [.. plan.Steps.Select(step => Row(step, new StepProgress(step.Index, StepStatus.Pending, [], null)))];
    }

    /// <summary>The plan being run.</summary>
    public PlanView Plan => _plan;

    /// <summary>One row per step.</summary>
    public IReadOnlyList<StepRow> Rows
    {
        get => _rows;
        private set
        {
            if (SetField(ref _rows, value))
            {
                OnPropertyChanged(nameof(DoneSteps));
                OnPropertyChanged(nameof(ProgressBasisPoints));
                OnPropertyChanged(nameof(FailedRow));
                OnPropertyChanged(nameof(RetryNote));
                OnPropertyChanged(nameof(SelectedLog));
            }
        }
    }

    /// <summary>The run's state.</summary>
    public RunState State
    {
        get => _state;
        private set
        {
            if (SetField(ref _state, value))
            {
                OnPropertyChanged(nameof(CanRetry));
                OnPropertyChanged(nameof(CanStart));
            }
        }
    }

    /// <summary>Estimated seconds left, from the engine.</summary>
    public ulong RemainingSeconds
    {
        get => _remainingSeconds;
        private set => SetField(ref _remainingSeconds, value);
    }

    /// <summary>How many steps are done.</summary>
    public int DoneSteps => Rows.Count(row => row.Status == StepStatus.Done);

    /// <summary>Progress in hundredths of a percent (10000 is all done).</summary>
    public int ProgressBasisPoints => Rows.Count == 0 ? 0 : (int)((long)DoneSteps * 10_000 / Rows.Count);

    /// <summary>The failed step, or null.</summary>
    public StepRow? FailedRow => Rows.FirstOrDefault(row => row.Status == StepStatus.Failed);

    /// <summary>The step whose log is shown; -1 means the failed step, else the running one, else the last.</summary>
    public int SelectedIndex
    {
        get => _selectedIndex;
        set
        {
            if (SetField(ref _selectedIndex, value))
            {
                OnPropertyChanged(nameof(SelectedLog));
            }
        }
    }

    /// <summary>The log of the selected step (see <see cref="SelectedIndex"/>).</summary>
    public IReadOnlyList<string> SelectedLog
    {
        get
        {
            var row = SelectedIndex >= 0 && SelectedIndex < Rows.Count
                ? Rows[SelectedIndex]
                : FailedRow
                    ?? Rows.FirstOrDefault(r => r.Status == StepStatus.Running)
                    ?? Rows.LastOrDefault(r => r.Status == StepStatus.Done)
                    ?? (Rows.Count > 0 ? Rows[0] : null);
            return row?.Log ?? [];
        }
    }

    /// <summary>True when Start works.</summary>
    public bool CanStart => State is RunState.NotStarted or RunState.Interrupted;

    /// <summary>True when Retry works: only after a failure.</summary>
    public bool CanRetry => State == RunState.Failed;

    /// <summary>What retrying will do, in plain words; null unless a step failed.</summary>
    public string? RetryNote => FailedRow is { } failed
        ? failed.NeedsCleanupBeforeRetry
            ? $"Step {failed.Index + 1} ({failed.Title}) changed the server in a way that cannot be repeated blindly. Retrying cleans it up first, then runs the step again. Steps already done are not repeated."
            : $"Retrying runs step {failed.Index + 1} ({failed.Title}) again. Steps already done are not repeated."
        : null;

    /// <summary>Shows the journal as it stands (after the app was reopened) without running anything.</summary>
    public void Seed(RunSnapshot snapshot)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        Apply(snapshot);
        if (State == RunState.Running)
        {
            State = RunState.Interrupted;
            Status = "This deployment was left part-way. Continue to finish it from the first unfinished step.";
        }
    }

    /// <summary>Starts the run, or continues it at the first unfinished step, and follows the journal until it stops.</summary>
    public async Task<bool> StartAsync(CancellationToken cancellationToken = default)
    {
        if (State is RunState.Running or RunState.Complete)
        {
            return false;
        }

        return await FollowAsync(cancellationToken);
    }

    /// <summary>Retries from the failed step. Refused unless a step failed.</summary>
    public async Task<bool> RetryAsync(CancellationToken cancellationToken = default)
    {
        if (!CanRetry)
        {
            return false;
        }

        return await FollowAsync(cancellationToken);
    }

    private async Task<bool> FollowAsync(CancellationToken cancellationToken)
    {
        State = RunState.Running;
        var ok = await TryAsync(async () =>
        {
            await foreach (var snapshot in Engine.RunAsync(_plan.PlanId, _signatures, cancellationToken))
            {
                Apply(snapshot);
            }
        });
        if (!ok && State == RunState.Running)
        {
            // The engine refused before it could report a failed step: the run did not move.
            State = Rows.Any(row => row.Status != StepStatus.Pending) ? RunState.Failed : RunState.NotStarted;
        }

        Status = State switch
        {
            RunState.Complete => "Every step finished.",
            RunState.Failed => FailedRow is { } failed ? $"Step {failed.Index + 1} failed: {failed.FailureReason}" : "The run stopped.",
            _ => Status,
        };
        return State == RunState.Complete;
    }

    private void Apply(RunSnapshot snapshot)
    {
        var byIndex = snapshot.Steps.ToDictionary(step => step.Index);
        Rows = [.. _plan.Steps.Select(step => Row(step, byIndex.TryGetValue(step.Index, out var progress)
            ? progress
            : new StepProgress(step.Index, StepStatus.Pending, [], null)))];
        RemainingSeconds = snapshot.RemainingSeconds;
        State = snapshot.Complete ? RunState.Complete
            : snapshot.Failed is not null ? RunState.Failed
            : snapshot.Steps.Any(step => step.Status != StepStatus.Pending) ? RunState.Running
            : RunState.NotStarted;
    }

    private static StepRow Row(PlanStepView step, StepProgress progress) => new(
        step.Index,
        step.Title,
        step.Host,
        progress.Status,
        progress.Status switch
        {
            StepStatus.Pending => "Waiting",
            StepStatus.Running => "Running",
            StepStatus.Done => "Done",
            _ => "Failed",
        },
        progress.Log,
        progress.FailureReason,
        step.NeedsCleanupBeforeRetry);
}
