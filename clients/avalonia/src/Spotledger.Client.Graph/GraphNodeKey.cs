namespace Spotledger.Client.Graph;

/// <summary>The identity of one record in a graph view: its entity and its record id.
/// The key is opaque to these views — nothing here parses it — and it is the stable
/// identity selection, automation ids and re-parenting are expressed in.</summary>
/// <param name="Entity">The entity (DocType) name.</param>
/// <param name="Id">The record id.</param>
public readonly record struct GraphNodeKey(string Entity, string Id)
{
    /// <summary>True when neither part carries a value (the default struct).</summary>
    public bool IsEmpty => string.IsNullOrEmpty(Entity) && string.IsNullOrEmpty(Id);

    /// <summary>True when both parts carry a value.</summary>
    public bool IsComplete => !string.IsNullOrEmpty(Entity) && !string.IsNullOrEmpty(Id);

    /// <inheritdoc />
    public override string ToString() => string.Concat(Entity, "/", Id);
}
