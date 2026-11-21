namespace Spotledger.Client.Analytics;

/// <summary>
/// A batch could not be handed to the endpoint. The message never carries an id, a token or the
/// endpoint URL: a refusal is reported by type, and the counters stay queued so nothing is lost.
/// <see cref="UsageAnalytics.FlushAsync"/> maps these to <see cref="FlushResult.Failed"/> (or
/// <see cref="FlushResult.NoIngestToken"/>) and keeps the batch for the next flush.
/// </summary>
public class AnalyticsTransportException : Exception
{
    public AnalyticsTransportException(string message)
        : base(message)
    {
    }

    public AnalyticsTransportException(string message, Exception inner)
        : base(message, inner)
    {
    }
}

/// <summary>
/// The effective request URI (base address plus path) is not <c>https</c> and the host did not
/// opt into a loopback exception. Nothing was sent; the batch stays queued.
/// </summary>
public sealed class AnalyticsEndpointNotSecureException : AnalyticsTransportException
{
    public AnalyticsEndpointNotSecureException()
        : base("Usage analytics require an https endpoint; nothing was sent.")
    {
    }

    public AnalyticsEndpointNotSecureException(string message)
        : base(message)
    {
    }

    public AnalyticsEndpointNotSecureException(string message, Exception inner)
        : base(message, inner)
    {
    }
}

/// <summary>No ingest token is configured, so the batch cannot be sent anonymously; nothing was sent.</summary>
public sealed class AnalyticsNoIngestTokenException : AnalyticsTransportException
{
    public AnalyticsNoIngestTokenException()
        : base("No analytics ingest token is configured; nothing was sent.")
    {
    }

    public AnalyticsNoIngestTokenException(string message)
        : base(message)
    {
    }

    public AnalyticsNoIngestTokenException(string message, Exception inner)
        : base(message, inner)
    {
    }
}

/// <summary>
/// A token was configured but is not shaped like one (characters outside
/// <c>[A-Za-z0-9._~+/=-]</c>, or a length outside 16..512). The value is never echoed.
/// </summary>
public sealed class AnalyticsInvalidIngestTokenException : AnalyticsTransportException
{
    public AnalyticsInvalidIngestTokenException()
        : base("The configured analytics ingest token is not well formed; nothing was sent.")
    {
    }

    public AnalyticsInvalidIngestTokenException(string message)
        : base(message)
    {
    }

    public AnalyticsInvalidIngestTokenException(string message, Exception inner)
        : base(message, inner)
    {
    }
}

/// <summary>
/// The endpoint answered with a 3xx. Redirects are never followed, so an <c>https</c> endpoint
/// cannot bounce a batch to <c>http</c>.
/// </summary>
public sealed class AnalyticsRedirectedException : AnalyticsTransportException
{
    public AnalyticsRedirectedException()
        : base("The analytics endpoint answered with a redirect; the batch was not delivered.")
    {
    }

    public AnalyticsRedirectedException(string message)
        : base(message)
    {
    }

    public AnalyticsRedirectedException(string message, Exception inner)
        : base(message, inner)
    {
    }
}

/// <summary>
/// The <see cref="HttpClient"/> handed to <see cref="HttpAnalyticsSink"/> would tie a batch to the
/// user (a default <c>Authorization</c> header, a cookie container, a redirect-following or
/// authentication-looking handler). Rejected at construction, before any event can be recorded.
/// </summary>
public sealed class AnalyticsClientNotAnonymousException : ArgumentException
{
    public AnalyticsClientNotAnonymousException(string message)
        : base(message)
    {
    }

    public AnalyticsClientNotAnonymousException(string message, string paramName)
        : base(message, paramName)
    {
    }

    public AnalyticsClientNotAnonymousException(string message, string paramName, Exception inner)
        : base(message, paramName, inner)
    {
    }
}
