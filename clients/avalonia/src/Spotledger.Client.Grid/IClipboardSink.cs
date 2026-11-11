namespace Spotledger.Client.Grid;

/// <summary>Row selection semantics for <c>SpotGrid</c>.</summary>
public enum GridSelectionMode
{
    /// <summary>One selected row at a time; a new click moves the selection.</summary>
    OneRow,

    /// <summary>Click selects, Ctrl-click toggles, Shift-click extends a range.</summary>
    Extended,

    /// <summary>Dragging or Shift-clicking selects contiguous ranges.</summary>
    Range,
}

/// <summary>
/// Clipboard abstraction so Ctrl+C's TSV copy is testable headless: the grid formats
/// and hands over; the host decides where it lands. No Avalonia clipboard type appears
/// in the grid (in headless runs there is no real clipboard).
/// </summary>
public interface IClipboardSink
{
    /// <summary>
    /// Places plain text on the clipboard. Implementations may forward to
    /// <c>IClipboard.SetTextAsync</c> in production; tests capture the string.
    /// </summary>
    Task SetTextAsync(string text, CancellationToken cancellationToken = default);

    /// <summary>The last text handed over (diagnostics; null before the first copy).</summary>
    string? LastText { get; }
}