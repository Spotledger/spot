using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>A candidate, never a seam name. The server resolves the field's lookup.</summary>
public sealed record LookupItem(string Id, string Label, string? Description = null, IReadOnlyDictionary<string, string>? Cells = null);

/// <summary>Lookup transport, including an authoritative saved-id resolution.</summary>
public interface ILookupSource
{
    Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken);
    Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken);
    /// <summary>Legacy sources retain label/description presentation without changes.</summary>
    async Task<LookupResult> SearchResultAsync(string text, CancellationToken cancellationToken)
    {
        var rows = await SearchAsync(text, cancellationToken);
        return new(rows.Select(row => new LookupResultRow(row.Id, row.Label, row.Description,
            row.Cells ?? new Dictionary<string, string>(), new Dictionary<string, string>())).ToArray(), [], false, null);
    }
}

/// <summary>Injectable debounce clock; production uses cancellable Task.Delay.</summary>
public interface ILookupDelay
{
    Task WaitAsync(TimeSpan delay, CancellationToken token);
}
internal sealed class LookupDelay : ILookupDelay
{
    public Task WaitAsync(TimeSpan delay, CancellationToken token) => Task.Delay(delay, token);
}
