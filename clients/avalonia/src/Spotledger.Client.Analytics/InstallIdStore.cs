namespace Spotledger.Client.Analytics;

/// <summary>
/// Where the install-random id lives between runs (the Shell keeps it in the user's settings).
/// Withdrawal discards it: <see cref="Discard"/> removes the stored value, and the next opt-in
/// creates a new id with <see cref="InstallId.Create"/>, so analytics cannot be linked across an
/// opt-out.
/// </summary>
public interface IInstallIdStore
{
    /// <summary>The stored id, or null when none is stored (or it was discarded).</summary>
    string? Read();

    /// <summary>Stores a newly created id.</summary>
    void Store(string installId);

    /// <summary>Forgets the stored id; a later opt-in starts a new identity.</summary>
    void Discard();
}

/// <summary>An <see cref="IInstallIdStore"/> in memory: the Shell's tests and the analytics tests use it.</summary>
public sealed class InMemoryInstallIdStore : IInstallIdStore
{
    private readonly object _gate = new();
    private string? _id;

    public InMemoryInstallIdStore()
    {
    }

    public InMemoryInstallIdStore(string? installId)
    {
        if (installId is not null && !InstallId.IsValid(installId))
        {
            throw new ArgumentException("The install id must be 32 lowercase hex characters (see InstallId.Create).", nameof(installId));
        }

        _id = installId;
    }

    /// <summary>The stored id, if any; for assertions about rotation.</summary>
    public string? Stored
    {
        get
        {
            lock (_gate)
            {
                return _id;
            }
        }
    }

    public static InMemoryInstallIdStore Seeded(string installId) => new(installId);

    public string? Read()
    {
        lock (_gate)
        {
            return _id;
        }
    }

    public void Store(string installId)
    {
        if (!InstallId.IsValid(installId))
        {
            throw new ArgumentException("The install id must be 32 lowercase hex characters (see InstallId.Create).", nameof(installId));
        }

        lock (_gate)
        {
            _id = installId;
        }
    }

    public void Discard()
    {
        lock (_gate)
        {
            _id = null;
        }
    }
}
