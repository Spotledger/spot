using Avalonia.Input;

namespace Spotledger.Client.Grid;

/// <summary>Type-ahead half of <see cref="SpotGrid"/>: buffers printable characters and
/// jumps the cell cursor to the first row whose sorted-column text starts with the
/// buffer (within the 1s typing window) — inside the applied window directly, beyond it
/// through a bounded one-row-at-a-time scan. Nothing here ever blocks on the source.</summary>
public partial class SpotGrid
{
    private readonly System.Text.StringBuilder _typeAhead = new();
    private DateTimeOffset _typeAheadStamp = DateTimeOffset.MinValue;
    private const double TypeAheadWindowMs = 1000;

    /// <summary>Buffers printable characters and jumps the cursor to the first visible
    /// row whose sorted-column text starts with the buffer (within 1s of typing).</summary>
    private void TypeAhead(string? keySymbol)
    {
        if (keySymbol is not { } text || text.Length == 0 || text.Contains('\n') || char.IsControl(text[0]))
        {
            return;
        }

        var now = DateTimeOffset.UtcNow;
        if ((now - _typeAheadStamp).TotalMilliseconds > TypeAheadWindowMs)
        {
            _typeAhead.Clear();
            _typeAheadAnchor = -1;
        }

        _typeAhead.Append(text);
        _typeAheadStamp = now;
        JumpTypeAhead(_typeAhead.ToString());
    }

    private int _typeAheadAnchor = -1;

    /// <summary>Moves the cursor to the first row of the sorted column whose label starts
    /// with the typed prefix — inside the applied window directly; beyond it through a
    /// bounded scan that fetches one row at a time (window [anchor, anchor+1) proves
    /// whether the sort really continues past). Fires from a timer-free pump loop.</summary>
    internal void JumpTypeAhead(string prefix)
    {
        if (_scheduler is not { } scheduler || SortState.Column < 0)
        {
            return;
        }

        // Inside the applied window: the direct scan.
        if (scheduler.LatestCompleted is { } applied
            && applied.Covers(FirstVisibleRow, Math.Max(1, VisibleRowCount())))
        {
            for (var i = 0; i < applied.Rows.Count; i++)
            {
                var row = applied.Rows[i];
                var cell = SortState.Column < row.Cells.Count ? row.Cells[SortState.Column] : null;
                var text = cell?.Text ?? string.Empty;
                if (text.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
                {
                    var sourceIndex = applied.Start + i;
                    CellCursor = new GridCursor(sourceIndex, SortState.Column, GridCursorOrigin.Keyboard);
                    EnsureCursorVisible();
                    InvalidateVisual();
                    _typeAheadAnchor = -1;
                    return;
                }
            }

            // Not inside the window from the current view start: try a continuation scan
            // past the window end (a sorted column may hold the match further down).
            _typeAheadAnchor = applied.Start + applied.Rows.Count;
            ScanForTypeAhead(prefix);
            return;
        }

        // The view sits outside the applied window (or none applied yet): probe from the
        // view start.
        _typeAheadAnchor = FirstVisibleRow;
        ScanForTypeAhead(prefix);
    }

    /// <summary>Fetches rows one at a time from <see cref="_typeAheadAnchor"/>, seeking a
    /// label that starts with the typed prefix; moves the cursor when found. Bounded by
    /// the source count and a step cap.</summary>
    private async void ScanForTypeAhead(string prefix)
    {
        if (_scheduler is not { } scheduler || _typeAheadAnchor < 0)
        {
            return;
        }

        var start = _typeAheadAnchor;
        var last = Math.Min(TotalRows - 1, start + 4000);
        for (var index = start; index <= last; index++)
        {
            if (_typeAhead.ToString() != prefix)
            {
                // The buffer changed under us (new keystrokes): this scan is stale.
                return;
            }

            var row = await scheduler.PeekRowAsync(index, CancellationToken.None).ConfigureAwait(true);
            if (row is null)
            {
                return;
            }

            var cell = SortState.Column < row.Cells.Count ? row.Cells[SortState.Column] : null;
            var text = cell?.Text ?? string.Empty;
            if (text.StartsWith(prefix, StringComparison.OrdinalIgnoreCase))
            {
                CellCursor = new GridCursor(index, SortState.Column, GridCursorOrigin.Keyboard);
                ScrollToRow(index);
                EnsureCursorVisible();
                InvalidateVisual();
                _typeAheadAnchor = -1;
                return;
            }
        }
    }

    /// <summary>The type-ahead buffer (tests read it between characters).</summary>
    internal string TypeAheadBuffer => _typeAhead.ToString();
}