namespace Spotledger.Client.Analytics;

/// <summary>The four events analytics knows. Nothing else can be reported.</summary>
public enum AnalyticsEvent
{
    ScreenOpened,
    ActionUsed,
    ErrorShown,
    ShortcutUsed,
}

/// <summary>
/// One aggregated counter. <see cref="ScreenId"/> is set for every event except
/// <see cref="AnalyticsEvent.ErrorShown"/>; <see cref="ItemId"/> is the action id, the error code
/// or the shortcut id, depending on <see cref="Event"/>, and is null for a screen opening.
/// </summary>
public sealed record AnalyticsCounter(AnalyticsEvent Event, string? ScreenId, string? ItemId, long Count);

/// <summary>
/// What is sent: counters only, under an install-random id and the day they were aggregated for.
/// <see cref="Dropped"/> counts events refused because the counter table was full;
/// <see cref="Rejected"/> counts screen ids the screens manifest does not list (a number: the
/// refused text itself is never kept).
/// </summary>
public sealed record AnalyticsPayload(
    int SchemaVersion,
    string InstallId,
    DateOnly Day,
    IReadOnlyList<AnalyticsCounter> Counters,
    long Dropped,
    long Rejected)
{
    public const int CurrentSchemaVersion = 1;

    public bool IsEmpty => Counters.Count == 0 && Dropped == 0 && Rejected == 0;
}

/// <summary>Where a batch goes. Production: <see cref="HttpAnalyticsSink"/>. Tests: <see cref="InMemoryAnalyticsSink"/>.</summary>
public interface IAnalyticsSink
{
    ValueTask SendAsync(AnalyticsPayload payload, CancellationToken cancellationToken);
}

/// <summary>Whether the user has opted in. Read on every event and every flush, so a change applies at once.</summary>
public interface IAnalyticsConsent
{
    bool IsEnabled { get; }
}

/// <summary>A consent switch for settings code and tests. Off by default: analytics are opt-in.</summary>
public sealed class AnalyticsConsent : IAnalyticsConsent
{
    private volatile bool _enabled;

    public AnalyticsConsent(bool enabled = false)
    {
        _enabled = enabled;
    }

    public bool IsEnabled
    {
        get => _enabled;
        set => _enabled = value;
    }
}

/// <summary>Keeps what it is sent; for tests.</summary>
public sealed class InMemoryAnalyticsSink : IAnalyticsSink
{
    private readonly List<AnalyticsPayload> _sent = [];
    private readonly object _gate = new();

    public IReadOnlyList<AnalyticsPayload> Sent
    {
        get
        {
            lock (_gate)
            {
                return [.. _sent];
            }
        }
    }

    public ValueTask SendAsync(AnalyticsPayload payload, CancellationToken cancellationToken)
    {
        ArgumentNullException.ThrowIfNull(payload);
        lock (_gate)
        {
            _sent.Add(payload);
        }

        return ValueTask.CompletedTask;
    }
}

/// <summary>The outcome of <see cref="UsageAnalytics.FlushAsync"/>.</summary>
public enum FlushResult
{
    /// <summary>Consent is off: nothing was sent and anything held was discarded.</summary>
    ConsentOff,

    /// <summary>Nothing was pending.</summary>
    NothingToSend,

    /// <summary>A batch was sent and its counters cleared.</summary>
    Sent,

    /// <summary>The sink failed (or refused: non-https endpoint, redirect); counters are kept for the next flush.</summary>
    Failed,

    /// <summary>No ingest token is configured, so nothing was sent; the counters stay local for a later flush.</summary>
    NoIngestToken,
}
