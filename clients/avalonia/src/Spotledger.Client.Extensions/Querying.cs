using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>The state of one data block: the shell draws a spinner, the rows, the empty
/// state or the error with its retry, and nothing else.</summary>
public enum PanelDataState
{
    /// <summary>Not loaded yet.</summary>
    Idle,
    /// <summary>A query is in flight.</summary>
    Loading,
    /// <summary>Rows (or an aggregate) arrived.</summary>
    Ready,
    /// <summary>The query answered with nothing.</summary>
    Empty,
    /// <summary>The query failed; <c>Problem</c> carries the text and the retry re-runs it.</summary>
    Error
}

/// <summary>One page of a rows query: where to start and how many rows to ask for.</summary>
/// <param name="Offset">The number of rows already held.</param>
/// <param name="Limit">The most rows to ask for.</param>
/// <param name="Filters">Server-side filters the app declared, when the panel passes any.</param>
public sealed record QueryPage(int Offset, int Limit, IReadOnlyDictionary<string, ExpressionValue>? Filters = null);

/// <summary>A rows query's answer. Cells are the server's display strings: the client never
/// re-parses a decimal into floating point.</summary>
/// <param name="Columns">The column names, when the server names them.</param>
/// <param name="Rows">The rows, each with one cell per column.</param>
/// <param name="HasMore">True when the query has rows after this page.</param>
public sealed record QueryRows(IReadOnlyList<string> Columns, IReadOnlyList<IReadOnlyList<string>> Rows, bool HasMore)
{
    /// <summary>An empty answer.</summary>
    public static QueryRows Empty { get; } = new([], [], false);
}

/// <summary>An aggregate query's answer: one expression value (int or exact decimal).</summary>
/// <param name="Value">The aggregate.</param>
public sealed record QueryAggregate(ExpressionValue Value);

/// <summary>
/// The seam between a panel's data blocks and the server: the real implementation issues
/// the app's declared query through the native API (so the user's permissions apply); a
/// fake in tests returns rows and aggregates synchronously or with an injected completion.
/// Every call is asynchronous and cancellable: a query never blocks the UI thread.
/// </summary>
public interface IQueryRunner
{
    /// <summary>Runs a declared rows query for one page.</summary>
    Task<QueryRows> RunRowsAsync(string query, QueryPage page, CancellationToken cancellationToken);

    /// <summary>Runs a declared aggregate query.</summary>
    Task<QueryAggregate> RunAggregateAsync(string query, CancellationToken cancellationToken);
}
