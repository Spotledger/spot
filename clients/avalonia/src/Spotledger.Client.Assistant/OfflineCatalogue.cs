namespace Spotledger.Client.Assistant;

/// <summary>A built-in explanation for an error code; shown when no AI is available.</summary>
public sealed record OfflineExplanation(string Code, string Title, string Explanation, string WhatToTry);

/// <summary>
/// The offline explanation catalogue (WP-PRD-13). It ships with the client, needs no
/// network and no model, and answers for the last error code the user saw.
/// </summary>
public sealed class OfflineCatalogue
{
    private readonly Dictionary<string, OfflineExplanation> _byCode;

    /// <summary>Creates a catalogue from explicit entries.</summary>
    public OfflineCatalogue(IEnumerable<OfflineExplanation> entries)
    {
        _byCode = entries.ToDictionary(e => e.Code, StringComparer.Ordinal);
    }

    /// <summary>The entries that ship with the client.</summary>
    public static OfflineCatalogue BuiltIn { get; } = new(
    [
        new("api.unauthorized", "You are signed out", "The server did not accept your session.", "Sign in again, then retry."),
        new("api.forbidden", "Not allowed", "Your role does not include this action.", "Ask an administrator for the permission."),
        new("jobs.stuck", "A background job is stuck", "A queued job has not moved for a long time.", "Ask an administrator to retry the job; nothing is lost."),
        new("cache.stale", "Showing old data", "A cached view is out of date.", "Refresh the view; if it persists, flush the cache."),
        new("api.unavailable", "The server is not reachable", "The client cannot reach the server.", "Check the connection; your unsaved work is kept."),
    ]);

    /// <summary>Looks up <paramref name="code"/>; null when the catalogue has no entry.</summary>
    public OfflineExplanation? Find(string? code) =>
        code is not null && _byCode.TryGetValue(code, out var e) ? e : null;

    /// <summary>The sentence the transcript shows for <paramref name="code"/>.</summary>
    public string Explain(string? code) => Find(code) is { } e
        ? $"{e.Title}. {e.Explanation} {e.WhatToTry}"
        : "The assistant is offline and has no built-in explanation for this error. You can ask a human.";
}
