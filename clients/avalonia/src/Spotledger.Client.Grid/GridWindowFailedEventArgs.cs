namespace Spotledger.Client.Grid;

/// <summary>Raised when a window fetch failed (an exception out of
/// <see cref="IGridSource.GetWindow"/> other than cancellation). The grid shows its
/// error row with retry; the data stays untouched until a retry lands.</summary>
/// <param name="request">The fetch that failed.</param>
/// <param name="error">The failure reason.</param>
public sealed class GridWindowFailedEventArgs(GridWindowRequest request, Exception error) : EventArgs
{
    /// <summary>The fetch that failed.</summary>
    public GridWindowRequest Request { get; } = request;

    /// <summary>The failure reason.</summary>
    public Exception Error { get; } = error;
}