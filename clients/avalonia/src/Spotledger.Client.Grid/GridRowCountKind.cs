namespace Spotledger.Client.Grid;

/// <summary>Whether an <see cref="IGridSource"/> knows its row count exactly.</summary>
public enum GridRowCountKind
{
    /// <summary>The count is exact; scrolling bounds and totals are final.</summary>
    Known,

    /// <summary>The count is an estimate; the grid trusts it for layout but treats
    /// short windows gracefully (fewer rows than requested shrink the extent).</summary>
    Estimated,

    /// <summary>The count is not known up front; the grid measures progress by the
    /// end of the last returned window instead of by the source's arithmetic.</summary>
    Unknown,
}