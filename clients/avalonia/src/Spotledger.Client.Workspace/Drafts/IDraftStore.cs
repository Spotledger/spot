namespace Spotledger.Client.Workspace;

/// <summary>What a save did. A save never throws for the failures a user can hit; the coordinator shows them quietly.</summary>
public enum DraftSaveOutcome
{
    /// <summary>The draft was written.</summary>
    Saved,

    /// <summary>The draft was kept in memory only, because nothing on this machine may hold a key.</summary>
    SavedInMemoryOnly,

    /// <summary>The draft is larger than the per-draft cap and was not written.</summary>
    TooLarge,

    /// <summary>The write failed (disk full, no permission); the old draft file is untouched.</summary>
    Failed,
}

/// <summary>The result of a save with the reason for a failure.</summary>
/// <param name="Outcome">What happened.</param>
/// <param name="Message">A short, plain reason for the status bar (no document content).</param>
public sealed record DraftSaveResult(DraftSaveOutcome Outcome, string? Message = null)
{
    /// <summary>True when the draft is stored, on disk or in memory.</summary>
    public bool IsStored => Outcome is DraftSaveOutcome.Saved or DraftSaveOutcome.SavedInMemoryOnly;
}

/// <summary>A draft file that could not be read and was set aside instead of crashing the client.</summary>
/// <param name="Scope">Whose file it was.</param>
/// <param name="FileName">The hashed file name it had.</param>
/// <param name="Reason">Why it was set aside, in plain words.</param>
/// <param name="AtUtc">When.</param>
public sealed record QuarantineNote(DraftScope Scope, string FileName, string Reason, DateTimeOffset AtUtc);

/// <summary>
/// Local storage for drafts, per user and per server. Implementations never throw for corruption or
/// disk trouble: a bad file is set aside with a note, a failed write is reported in the result.
/// </summary>
public interface IDraftStore
{
    /// <summary>True when drafts of <paramref name="scope"/> reach the disk. False means memory only.</summary>
    Task<bool> IsPersistentAsync(DraftScope scope, CancellationToken cancellationToken = default);

    /// <summary>The line to show when drafts are not saved to disk (null when they are).</summary>
    string? MemoryOnlyNotice { get; }

    /// <summary>Writes (replaces) the draft atomically.</summary>
    Task<DraftSaveResult> SaveAsync(DraftRecord record, CancellationToken cancellationToken = default);

    /// <summary>Reads a draft; null when there is none or it could not be read (then it was quarantined).</summary>
    Task<DraftRecord?> LoadAsync(DraftKey key, CancellationToken cancellationToken = default);

    /// <summary>Removes a draft (after a save, a discard). Removing an absent draft is not an error.</summary>
    Task DeleteAsync(DraftKey key, CancellationToken cancellationToken = default);

    /// <summary>Lists every readable draft of the scope, newest first (the "Recover drafts" list).</summary>
    Task<IReadOnlyList<DraftSummary>> ListAsync(DraftScope scope, CancellationToken cancellationToken = default);

    /// <summary>Removes every draft of the scope and its key (sign-out of that user on that server).</summary>
    Task DeleteAllAsync(DraftScope scope, CancellationToken cancellationToken = default);

    /// <summary>The files set aside so far in this process.</summary>
    IReadOnlyList<QuarantineNote> Quarantined { get; }

    /// <summary>Raised when a file is set aside.</summary>
    event EventHandler<QuarantineNote>? DraftQuarantined;
}
