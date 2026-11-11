using System.Globalization;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Workspace;

/// <summary>Where the recovery bar is.</summary>
public enum RecoveryBarState
{
    /// <summary>No draft to offer, or the offer was answered.</summary>
    Hidden,

    /// <summary>"Restore unsaved changes from ..." or "Discard".</summary>
    Offering,

    /// <summary>The server's version moved on: the changes the server made are listed and the user chooses.</summary>
    Reviewing,
}

/// <summary>
/// The non-modal bar a form shows when a draft exists for the document it opened. Restoring into a
/// document whose version has not changed applies the draft at once as ordinary unsaved edits. When
/// the server's version moved on, the bar first lists what changed on the server among the fields the
/// draft edits, and the user applies everything, applies only what the server did not touch, or
/// discards. Nothing is ever sent from here: a restore only edits the draft, which the user reviews
/// and saves like any change.
/// </summary>
public sealed class RecoveryBarViewModel : ObservableObject
{
    private readonly IDraftStore _store;
    private readonly AutosaveCoordinator? _coordinator;
    private readonly DocumentDraft _draft;
    private readonly Func<DateTimeOffset, string> _formatTime;
    private RecoveryBarState _state = RecoveryBarState.Offering;
    private string? _result;
    private readonly Func<bool>? _isDocumentReadOnly;

    /// <summary>Creates the bar for a stored draft opened against <paramref name="draft"/>.</summary>
    public RecoveryBarViewModel(
        IDraftStore store,
        DraftRecord record,
        DocumentDraft draft,
        AutosaveCoordinator? coordinator = null,
        Func<DateTimeOffset, string>? formatTime = null,
        Func<bool>? isDocumentReadOnly = null)
    {
        _isDocumentReadOnly = isDocumentReadOnly;
        _store = store ?? throw new ArgumentNullException(nameof(store));
        Record = record ?? throw new ArgumentNullException(nameof(record));
        _draft = draft ?? throw new ArgumentNullException(nameof(draft));
        _coordinator = coordinator;
        _formatTime = formatTime ?? (t => t.ToLocalTime().ToString("g", CultureInfo.CurrentCulture));
        Plan = DraftSnapshot.Plan(record, draft, isDocumentReadOnly?.Invoke() == true);
        RestoreCommand = new AsyncRelayCommand(_ => RestoreAsync());
        ApplyAllCommand = new AsyncRelayCommand(_ => ApplyAsync(includeConflicts: true), _ => State == RecoveryBarState.Reviewing);
        ApplyUntouchedCommand = new AsyncRelayCommand(_ => ApplyAsync(includeConflicts: false), _ => State == RecoveryBarState.Reviewing);
        DiscardCommand = new AsyncRelayCommand(_ => DiscardAsync());
    }

    /// <summary>Looks for a draft of the document that <paramref name="draft"/> holds; null when there is none.</summary>
    public static async Task<RecoveryBarViewModel?> TryCreateAsync(
        IDraftStore store,
        DraftScope scope,
        string docType,
        DocumentDraft draft,
        AutosaveCoordinator? coordinator = null,
        Func<DateTimeOffset, string>? formatTime = null,
        Func<bool>? isDocumentReadOnly = null)
    {
        ArgumentNullException.ThrowIfNull(store);
        ArgumentNullException.ThrowIfNull(draft);
        if (draft.DocumentId is null)
        {
            return null;
        }

        var record = await store.LoadAsync(DraftKey.ForDocument(scope, docType, draft.DocumentId)).ConfigureAwait(false);
        return record is null || record.IsEmpty ? null : new RecoveryBarViewModel(store, record, draft, coordinator, formatTime, isDocumentReadOnly);
    }

    /// <summary>The stored draft on offer.</summary>
    public DraftRecord Record { get; }

    /// <summary>What a restore would do against the document as loaded.</summary>
    public RestorePlan Plan { get; }

    /// <summary>Where the bar is.</summary>
    public RecoveryBarState State
    {
        get => _state;
        private set
        {
            if (SetField(ref _state, value))
            {
                OnPropertyChanged(nameof(IsVisible));
                OnPropertyChanged(nameof(IsReviewing));
            }
        }
    }

    /// <summary>True while the bar shows.</summary>
    public bool IsVisible => State != RecoveryBarState.Hidden;

    /// <summary>True while the server-changes list shows.</summary>
    public bool IsReviewing => State == RecoveryBarState.Reviewing;

    /// <summary>"Restore unsaved changes from 14:05".</summary>
    public string Message => "Restore unsaved changes from " + _formatTime(Record.SavedAtUtc);

    /// <summary>The text of the Restore button.</summary>
    public string RestoreLabel => Plan.NeedsReview ? "Review" : "Restore";

    /// <summary>The text of the Discard button.</summary>
    public string DiscardLabel => "Discard";

    /// <summary>What changed on the server among the fields the draft edits.</summary>
    public IReadOnlyList<RestoreItem> ServerChanges => Plan.Conflicts;

    /// <summary>Fields that cannot be restored (removed or no longer visible to this user), with the reason.</summary>
    public IReadOnlyList<RestoreItem> Skipped => [.. Plan.Items.Where(i => i.SkipReason is not null)];

    /// <summary>What the last answer did, for a live-region announcement.</summary>
    public string? Result
    {
        get => _result;
        private set => SetField(ref _result, value);
    }

    /// <summary>Restore (or open the review when the server changed).</summary>
    public AsyncRelayCommand RestoreCommand { get; }

    /// <summary>Review step: apply every change, the draft's value winning on conflicts.</summary>
    public AsyncRelayCommand ApplyAllCommand { get; }

    /// <summary>Review step: apply only the fields the server did not change.</summary>
    public AsyncRelayCommand ApplyUntouchedCommand { get; }

    /// <summary>Delete the stored draft.</summary>
    public AsyncRelayCommand DiscardCommand { get; }

    private Task RestoreAsync()
    {
        if (Plan.NeedsReview && Plan.Conflicts.Count > 0)
        {
            State = RecoveryBarState.Reviewing;
            ApplyAllCommand.RaiseCanExecuteChanged();
            ApplyUntouchedCommand.RaiseCanExecuteChanged();
            return Task.CompletedTask;
        }

        return ApplyAsync(includeConflicts: true);
    }

    private Task ApplyAsync(bool includeConflicts)
    {
        // The edit gate is checked again at the moment of applying: the document may have become read-only.
        if (_isDocumentReadOnly?.Invoke() == true)
        {
            Result = "This document is read-only, so nothing was restored.";
            State = RecoveryBarState.Hidden;
            return Task.CompletedTask;
        }

        var applied = DraftSnapshot.Apply(Plan, _draft, includeConflicts);
        Result = applied.Count == 1 ? "Restored 1 change." : $"Restored {applied.Count} changes.";
        State = RecoveryBarState.Hidden;
        return Task.CompletedTask;
    }

    private async Task DiscardAsync()
    {
        if (_coordinator is not null)
        {
            await _coordinator.DiscardAsync(_draft).ConfigureAwait(true);
        }

        await _store.DeleteAsync(Record.Key).ConfigureAwait(true);
        Result = "Discarded the unsaved changes.";
        State = RecoveryBarState.Hidden;
    }
}
