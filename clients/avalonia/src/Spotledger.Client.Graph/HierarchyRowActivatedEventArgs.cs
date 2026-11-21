namespace Spotledger.Client.Graph;

/// <summary>The row an activation, expansion or collapse happened on.</summary>
public sealed class HierarchyRowActivatedEventArgs : EventArgs
{
    /// <summary>Creates the event arguments for one row.</summary>
    /// <param name="row">The row.</param>
    /// <param name="index">Its visible index.</param>
    public HierarchyRowActivatedEventArgs(HierarchyRow row, int index)
    {
        Row = row;
        Index = index;
    }

    /// <summary>The row.</summary>
    public HierarchyRow Row { get; }

    /// <summary>The row's key.</summary>
    public GraphNodeKey Key => Row.Key;

    /// <summary>The row's visible index.</summary>
    public int Index { get; }
}
