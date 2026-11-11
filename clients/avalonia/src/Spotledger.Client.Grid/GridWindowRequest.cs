namespace Spotledger.Client.Grid;

/// <summary>An immutable record of one window fetch the <see cref="WindowScheduler"/>
/// issued to an <see cref="IGridSource"/>. The <see cref="Sequence"/> strictly increases;
/// a completed fetch whose sequence no longer matches the scheduler's current sequence
/// is stale and is dropped, never applied.</summary>
/// <param name="Sequence">Monotonic id of this fetch.</param>
/// <param name="Start">First source row index requested.</param>
/// <param name="Length">Number of rows requested (never more than view + prefetch margin).</param>
// CA2231: positional records already synthesize ==/!= operators consistent with Equals.
public sealed record GridWindowRequest(int Sequence, int Start, int Length)
{
    /// <inheritdoc />
    public override string ToString() => $"#{Sequence} [{Start}, {Start + Length})";
}

/// <summary>Raised when a window fetch completed and was applied by the
/// <see cref="WindowScheduler"/>. <see cref="Start"/> plus <see cref="Rows.Count"/> is
/// the covered source range; <see cref="Covers(int, int)"/> answers whether a view
/// falls inside it.</summary>
/// <param name="sequence">The sequence of the fetch that completed.</param>
/// <param name="start">The first covered source row index.</param>
/// <param name="rows">The fetched rows in source order (may be shorter than requested
/// when the source ran out, which is how an unknown count is discovered).</param>
public sealed class GridWindowCompletedEventArgs(
    int sequence, int start, IReadOnlyList<GridRow> rows) : EventArgs
{
    /// <summary>The sequence of the fetch that completed.</summary>
    public int Sequence { get; } = sequence;

    /// <summary>The first covered source row index.</summary>
    public int Start { get; } = start;

    /// <summary>The fetched rows in source order (may be shorter than requested when
    /// the source ran out, which is how an unknown count is discovered).</summary>
    public IReadOnlyList<GridRow> Rows { get; } = rows;
    /// <summary>True when every row of <c>[viewStart, viewStart + viewLength)</c> is
    /// present in this window.</summary>
    public bool Covers(int viewStart, int viewLength)
        => Start <= viewStart && Start + Rows.Count >= viewStart + viewLength;

    /// <summary>The row at <paramref name="sourceIndex"/>, or null when outside.</summary>
    public GridRow? RowAt(int sourceIndex)
    {
        var offset = sourceIndex - Start;
        return offset >= 0 && offset < Rows.Count ? Rows[offset] : null;
    }
}