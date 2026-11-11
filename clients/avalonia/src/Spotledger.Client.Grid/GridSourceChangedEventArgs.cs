namespace Spotledger.Client.Grid;

/// <summary>What changed in an <see cref="IGridSource"/> when <see cref="IGridSource.Changed"/>
/// fires. The grid folds any of these into a cache repair: a full reset drops everything,
/// an insert or removal shifts cached windows.</summary>
public enum GridChangeKind
{
    /// <summary>Ordering or contents changed wholesale (sort, filter, reload).</summary>
    Reset,

    /// <summary>Rows were inserted at <see cref="GridSourceChangedEventArgs.Index"/>;
    /// <see cref="GridSourceChangedEventArgs.Count"/> rows were added.</summary>
    RowsInserted,

    /// <summary>Rows were removed at <see cref="GridSourceChangedEventArgs.Index"/>;
    /// <see cref="GridSourceChangedEventArgs.Count"/> rows were dropped.</summary>
    RowsRemoved,
}

/// <summary>The payload of <see cref="IGridSource.Changed"/>.</summary>
/// <param name="kind">What happened.</param>
/// <param name="index">The first affected row index (0 for a reset).</param>
/// <param name="count">How many rows the change touched (0 for a reset).</param>
public sealed class GridSourceChangedEventArgs(
    GridChangeKind kind, int index = 0, int count = 0) : EventArgs
{
    /// <summary>What happened.</summary>
    public GridChangeKind Kind { get; } = kind;

    /// <summary>The first affected row index (0 for a reset).</summary>
    public int Index { get; } = index;

    /// <summary>How many rows the change touched (0 for a reset).</summary>
    public int Count { get; } = count;
}