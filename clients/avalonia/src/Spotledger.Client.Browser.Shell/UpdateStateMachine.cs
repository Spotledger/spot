namespace Spotledger.Client.Browser.Shell;

/// <summary>Where a shell update is.</summary>
public enum UpdateState
{
    /// <summary>No update is known.</summary>
    Idle,

    /// <summary>A newer build exists and is being downloaded.</summary>
    UpdateFound,

    /// <summary>The newer build is downloaded and verified; it activates on the next load.</summary>
    UpdateReady,

    /// <summary>Activation was requested; the page is about to reload on the new build.</summary>
    Applying,
}

/// <summary>The answer to <see cref="UpdateStateMachine.RequestApply"/>.</summary>
public enum ApplyOutcome
{
    /// <summary>No update is ready.</summary>
    NotReady,

    /// <summary>The update is being applied now.</summary>
    Started,

    /// <summary>The app has unsaved edits: the request is remembered and applied when they are gone.</summary>
    Deferred,
}

/// <summary>
/// The update state machine of the shell. A new build is downloaded in the background and activates on the
/// next load; the prompt offering it is shown only while the app has no unsaved edits, and an apply is never
/// started while it has. Until the app has reported its edit state at least once, edits are assumed to exist
/// (fail closed): an app that never reports simply gets the new build on its next load. Time is not used.
/// </summary>
public sealed class UpdateStateMachine
{
    private bool _editsKnown;
    private bool _hasUnsavedEdits;
    private bool _dismissed;
    private bool _applyRequested;

    /// <summary>Creates the machine for the build that is running.</summary>
    public UpdateStateMachine(string currentBuild)
    {
        CurrentBuild = currentBuild;
    }

    /// <summary>Raised with the new build id when the machine enters <see cref="UpdateState.Applying"/>.</summary>
    public event Action<string>? ApplyStarted;

    /// <summary>The build that is running.</summary>
    public string CurrentBuild { get; private set; }

    /// <summary>The build being downloaded or waiting; null when <see cref="State"/> is Idle.</summary>
    public string? PendingBuild { get; private set; }

    /// <summary>The state.</summary>
    public UpdateState State { get; private set; } = UpdateState.Idle;

    /// <summary>True when the app has unsaved edits, or has not said it has none.</summary>
    public bool EditsBlockApply => !_editsKnown || _hasUnsavedEdits;

    /// <summary>True when the dismissible prompt should be on screen.</summary>
    public bool PromptVisible => State == UpdateState.UpdateReady && !_dismissed && !EditsBlockApply;

    /// <summary>A newer build was seen (the service worker started downloading it).</summary>
    public void OnUpdateFound(string build)
    {
        if (State == UpdateState.Applying
            || string.Equals(build, CurrentBuild, StringComparison.Ordinal)
            || (State != UpdateState.Idle && string.Equals(build, PendingBuild, StringComparison.Ordinal)))
        {
            return;
        }

        PendingBuild = build;
        State = UpdateState.UpdateFound;
        _dismissed = false;
        _applyRequested = false;
    }

    /// <summary>The new build is downloaded and verified.</summary>
    public void OnStaged(string build)
    {
        if (State == UpdateState.UpdateFound && string.Equals(build, PendingBuild, StringComparison.Ordinal))
        {
            State = UpdateState.UpdateReady;
        }
    }

    /// <summary>The download failed; the machine goes back to idle and the next check tries again.</summary>
    public void OnStageFailed(string build)
    {
        if (State == UpdateState.UpdateFound && string.Equals(build, PendingBuild, StringComparison.Ordinal))
        {
            State = UpdateState.Idle;
            PendingBuild = null;
        }
    }

    /// <summary>The app reports whether it has unsaved edits. A deferred apply starts when the last edit is gone.</summary>
    public void SetUnsavedEdits(bool hasUnsavedEdits)
    {
        _editsKnown = true;
        _hasUnsavedEdits = hasUnsavedEdits;
        if (!hasUnsavedEdits && _applyRequested && State == UpdateState.UpdateReady)
        {
            BeginApply();
        }
    }

    /// <summary>The user chose "update now" (or the app asks for it).</summary>
    public ApplyOutcome RequestApply()
    {
        if (State != UpdateState.UpdateReady)
        {
            return ApplyOutcome.NotReady;
        }

        if (EditsBlockApply)
        {
            _applyRequested = true;
            return ApplyOutcome.Deferred;
        }

        BeginApply();
        return ApplyOutcome.Started;
    }

    /// <summary>The user hid the prompt. The build still activates on the next load.</summary>
    public void Dismiss() => _dismissed = true;

    /// <summary>The new build is now active (the page reloaded on it).</summary>
    public void OnActivated(string build)
    {
        if (State == UpdateState.Applying && string.Equals(build, PendingBuild, StringComparison.Ordinal))
        {
            CurrentBuild = build;
            PendingBuild = null;
            State = UpdateState.Idle;
            _applyRequested = false;
            _dismissed = false;
        }
    }

    private void BeginApply()
    {
        _applyRequested = false;
        State = UpdateState.Applying;
        ApplyStarted?.Invoke(PendingBuild ?? string.Empty);
    }
}
