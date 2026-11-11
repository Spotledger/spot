namespace Spotledger.Client.Grid;

/// <summary>
/// The data contract the grid renders against. The grid never holds the data set: it asks
/// for windows of rows as they scroll into view (plus a prefetch margin), cancels windows
/// that scroll back out before they complete, and pushes sorts down instead of sorting
/// locally. Everything is awaitable so no call ever blocks the UI thread.
/// </summary>
public interface IGridSource
{
    /// <summary>The source's statement about its row count (known, estimated or unknown).</summary>
    GridRowCount Count { get; }

    /// <summary>Fetches <paramref name="length"/> rows starting at <paramref name="start"/>.
    /// Implementations must return a completed or completed-soon task; the grid cancels the
    /// token when a newer window supersedes this one, and honoring the cancellation quickly
    /// is what keeps prefetch churn cheap. The returned list must contain
    /// <c>min(length, rowsAvailable)</c> rows in source order.</summary>
    Task<IReadOnlyList<GridRow>> GetWindow(int start, int length, CancellationToken cancellationToken);

    /// <summary>Sorts by the data column at <paramref name="columnIndex"/> in
    /// <paramref name="direction"/> (the grid passes <see cref="GridSortDirection.None"/> to
    /// clear). Implementations report completion by raising <see cref="Changed"/> with a
    /// reset afterwards.</summary>
    Task Sort(int columnIndex, GridSortDirection direction);

    /// <summary>Raised when the row set changes underneath the grid (reset, insert, removal).
    /// The grid repairs its cache and re-requests the visible window.</summary>
    event EventHandler<GridSourceChangedEventArgs>? Changed;
}