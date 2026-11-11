namespace Spotledger.Client.Analytics;

/// <summary>
/// Yields the <em>ingest token</em>: a value issued to this deployment by the telemetry endpoint
/// owner (see tools/ui-coverage/analytics-events.md). It is deliberately not the user's session
/// token, API key or any credential of the user, so a batch cannot be tied to a person. Returning
/// null or an empty string (no ingest token configured) makes the sink refuse to send and report
/// <see cref="FlushResult.NoIngestToken"/>; the counters stay local.
/// </summary>
public interface IAnalyticsTokenSource
{
    /// <summary>The ingest token, or null/empty when the deployment has none.</summary>
    string? GetIngestToken();
}

/// <summary>
/// A fixed ingest token, for a host that reads it from deployment settings once. Null or empty
/// means "not configured", which is the default: an install with no token records locally only.
/// </summary>
public sealed class FixedAnalyticsTokenSource : IAnalyticsTokenSource
{
    private readonly string? _token;

    public FixedAnalyticsTokenSource(string? token)
    {
        _token = token;
    }

    public static FixedAnalyticsTokenSource None { get; } = new(null);

    public string? GetIngestToken() => _token;
}
