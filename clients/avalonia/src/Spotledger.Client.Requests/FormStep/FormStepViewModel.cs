using System.Collections.ObjectModel;
using System.Net.Http;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Requests;

/// <summary>Where a form step is.</summary>
public enum StepPhase
{
    /// <summary>Loading the step and its draft.</summary>
    Loading,

    /// <summary>Open for the person to fill in.</summary>
    Open,

    /// <summary>The step was completed.</summary>
    Completed,

    /// <summary>The step is not open (anymore) or could not be loaded.</summary>
    Unavailable,
}

/// <summary>
/// A form step: the generic document form bound to the run's draft record, with the step's title,
/// instructions and who the run is waiting for, a Save and a Complete button, and the server's
/// validation errors mapped to fields. Editing and saving the draft is the form's own work (so the
/// draft goes through the record type's lifecycle and the workspace autosave sees every change);
/// this model only completes the step. The server stays the authority: a failed completion keeps
/// the step open and shows what to fix.
/// </summary>
public sealed class FormStepViewModel : ObservableObject, IDisposable
{
    private readonly IRequestsGateway _requests;
    private readonly string _taskId;
    private readonly Func<StepInfo, DocumentFormViewModel> _formFactory;
    private StepPhase _phase = StepPhase.Loading;
    private StepInfo? _info;
    private DocumentFormViewModel? _form;
    private string? _status;
    private bool _busy;

    /// <summary>Creates the step for <paramref name="taskId"/>. <paramref name="formFactory"/> builds the
    /// form over the step's draft record (the host passes its gateway, shell and autosave).</summary>
    public FormStepViewModel(
        IRequestsGateway requests,
        string taskId,
        Func<StepInfo, DocumentFormViewModel> formFactory,
        RequestText text)
    {
        _requests = requests ?? throw new ArgumentNullException(nameof(requests));
        ArgumentException.ThrowIfNullOrWhiteSpace(taskId);
        _taskId = taskId;
        _formFactory = formFactory ?? throw new ArgumentNullException(nameof(formFactory));
        Text = text ?? throw new ArgumentNullException(nameof(text));
    }

    /// <summary>The texts.</summary>
    public RequestText Text { get; }

    /// <summary>Where the step is.</summary>
    public StepPhase Phase
    {
        get => _phase;
        private set
        {
            if (SetField(ref _phase, value))
            {
                OnPropertyChanged(nameof(CanComplete));
            }
        }
    }

    /// <summary>The server's description of the step.</summary>
    public StepInfo? Info => _info;

    /// <summary>The generic form over the draft record, once loaded.</summary>
    public DocumentFormViewModel? Form => _form;

    /// <summary>The step's title.</summary>
    public string Title => _info?.Title ?? string.Empty;

    /// <summary>What the person should do.</summary>
    public string? Instructions => _info?.Instructions;

    /// <summary>"Waiting for X" when the run waits for somebody else, else null.</summary>
    public string? WaitingNotice => _info?.WaitingFor is { Length: > 0 } person
        ? Text.Get("step.waiting", ("person", person))
        : null;

    /// <summary>The save button text.</summary>
    public string SaveLabel => Text.Get("step.save");

    /// <summary>The complete button text.</summary>
    public string CompleteLabel => Text.Get("step.complete");

    /// <summary>A short line after an action (saved, invalid, closed), or null.</summary>
    public string? Status
    {
        get => _status;
        private set => SetField(ref _status, value);
    }

    /// <summary>True while a call is in flight.</summary>
    public bool IsBusy
    {
        get => _busy;
        private set
        {
            if (SetField(ref _busy, value))
            {
                OnPropertyChanged(nameof(CanComplete));
            }
        }
    }

    /// <summary>Complete is available while the step is open and nothing is in flight.</summary>
    public bool CanComplete => Phase == StepPhase.Open && !IsBusy;

    /// <summary>Raised when the step was completed, so the host can close the tab or open the next step.</summary>
    public event EventHandler? Completed;

    /// <summary>Loads the step, then its draft record into the form.</summary>
    public async Task LoadAsync(CancellationToken cancellationToken = default)
    {
        try
        {
            _info = await _requests.GetStepAsync(_taskId, cancellationToken).ConfigureAwait(true);
            _form = _formFactory(_info);
            await _form.LoadAsync(cancellationToken).ConfigureAwait(true);
            Phase = _form.Phase == FormPhase.Ready ? StepPhase.Open : StepPhase.Unavailable;
        }
        catch (ApiRequestException e)
        {
            Phase = StepPhase.Unavailable;
            Status = StepNotOpen(e.Error) ? Text.Get("step.not_open") : Text.Get("error.generic");
        }

        OnPropertyChanged(nameof(Info));
        OnPropertyChanged(nameof(Form));
        OnPropertyChanged(nameof(Title));
        OnPropertyChanged(nameof(Instructions));
        OnPropertyChanged(nameof(WaitingNotice));
    }

    /// <summary>Saves the draft through the form. Returns whether it was saved.</summary>
    public async Task<bool> SaveAsync()
    {
        if (_form is null || Phase != StepPhase.Open)
        {
            return false;
        }

        var saved = await _form.SaveAsync().ConfigureAwait(true);
        Status = saved ? Text.Get("step.saved") : Text.Get("step.invalid");
        return saved;
    }

    /// <summary>Saves the draft, then completes the step. A refusal keeps the step open: field errors
    /// land on the form's fields and the rest in its message area.</summary>
    public async Task<bool> CompleteAsync(CancellationToken cancellationToken = default)
    {
        if (_form is null || !CanComplete)
        {
            return false;
        }

        IsBusy = true;
        try
        {
            if (!await SaveAsync().ConfigureAwait(true))
            {
                return false;
            }

            await _requests.CompleteStepAsync(_taskId, cancellationToken).ConfigureAwait(true);
            Phase = StepPhase.Completed;
            Status = null;
            Completed?.Invoke(this, EventArgs.Empty);
            return true;
        }
        catch (ApiRequestException e)
        {
            ShowCompletionError(e.Error);
            return false;
        }
        catch (Exception e) when (e is HttpRequestException or OperationCanceledException)
        {
            Status = Text.Get("error.generic");
            return false;
        }
        finally
        {
            IsBusy = false;
        }
    }

    /// <inheritdoc />
    public void Dispose() => _form?.Dispose();

    private static bool StepNotOpen(ApiError error) =>
        string.Equals(error.Code, "bpm_entry.step_not_open", StringComparison.Ordinal);

    private void ShowCompletionError(ApiError error)
    {
        if (StepNotOpen(error))
        {
            Phase = StepPhase.Unavailable;
            Status = Text.Get("step.not_open");
            return;
        }

        Status = Text.Get("step.invalid");
        if (_form?.Draft is { } draft)
        {
            var applied = ErrorMapper.Apply(draft, error);
            foreach (var message in applied.DocumentMessages)
            {
                _form.Messages.Add(new FormMessage("error", message.Code, message.Message));
            }

            _form.FocusFirstInvalid();
        }
    }

    /// <summary>The field problems the form currently shows, by field (for a screen reader summary and tests).</summary>
    public IReadOnlyDictionary<string, IReadOnlyList<string>> FieldProblems()
    {
        var result = new Dictionary<string, IReadOnlyList<string>>(StringComparer.Ordinal);
        if (_form?.Draft is { } draft)
        {
            foreach (var (name, state) in draft.Fields)
            {
                if (state.Errors.Count > 0)
                {
                    result[name] = state.Errors.Select(e => e.Message).ToArray();
                }
            }
        }

        return result;
    }

    /// <summary>The form's messages, for the view.</summary>
    public ObservableCollection<FormMessage>? Messages => _form?.Messages;
}
