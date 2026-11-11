namespace Spotledger.Client.Controls;

/// <summary>
/// One choice in a <see cref="SpotSelect"/>. <see cref="ToString"/> returns the label so a
/// plain <c>ContentPresenter</c> shows the right text even in the selection box.
/// </summary>
/// <param name="Value">The machine value stored by the form.</param>
/// <param name="Label">The user-visible text (RTL-safe; supplied by the caller).</param>
public sealed record SelectOption(object Value, string Label)
{
    public override string ToString() => Label;
}
