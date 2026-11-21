using Avalonia;
using Spotledger.Client.Controls;

namespace Spotledger.Client.Graph;

/// <summary>Infrastructure half of <see cref="SpotHierarchy"/>: viewport and scroll math,
/// token metrics, the async page-load launcher and the initial load. Every fetch runs off
/// the input path and reports failure into the error band instead of throwing at a caller.</summary>
public partial class SpotHierarchy
{
    private double _viewportWidth;
    private double _viewportHeight;
    private double _scrollRows;

    internal string? ErrorState => _error;
    private string? _error;

    internal bool IsCompact => Density.GetMode(this) == DensityMode.Compact;

    internal double RowHeight => GraphTokens.Number(
        IsCompact ? "Density.Compact.Grid.RowHeight" : "Density.Comfortable.Grid.RowHeight",
        40);

    internal double ViewportHeight => _viewportHeight;

    internal double ViewportWidth => _viewportWidth;

    internal double ScrollRows => _scrollRows;

    internal int FirstVisibleRow => (int)Math.Floor(_scrollRows);

    internal int VisibleRowCount()
    {
        var height = _viewportHeight;
        return RowHeight <= 0 ? 0 : (int)Math.Ceiling(height / RowHeight);
    }

    internal int TotalRows => _model?.RowCount ?? 0;

    internal double MaxScrollRows => Math.Max(0, TotalRows - VisibleRowCount());

    internal bool IsRtl => FlowDirection == Avalonia.Media.FlowDirection.RightToLeft;

    internal double BreadcrumbHeight => RowHeight;

    internal double ContentTop => ShowBreadcrumb ? BreadcrumbHeight : 0;

    internal double BodyHeightFor(double totalHeight) => Math.Max(0, totalHeight - ContentTop);

    internal void SetViewport(double width, double bodyHeight)
    {
        _viewportWidth = Math.Max(0, width);
        _viewportHeight = Math.Max(0, bodyHeight);
        _scrollRows = Math.Clamp(_scrollRows, 0, MaxScrollRows);
    }

    internal int RowIndexAt(double y)
    {
        if (RowHeight <= 0 || y < ContentTop || y > ContentTop + _viewportHeight)
        {
            return -1;
        }

        var index = FirstVisibleRow + (int)Math.Floor((y - ContentTop) / RowHeight);
        return index >= 0 && index < TotalRows ? index : -1;
    }

    internal double RowY(int index) => ContentTop + ((index - _scrollRows) * RowHeight);

    internal void ScrollToRow(double rows)
    {
        var clamped = Math.Clamp(rows, 0, MaxScrollRows);
        if (Math.Abs(clamped - _scrollRows) < 0.001)
        {
            return;
        }

        _scrollRows = clamped;
        InvalidateVisual();
        Start(LoadMoreForViewportAsync);
    }

    internal void ScrollBy(double delta) => ScrollToRow(_scrollRows + delta);

    internal void RevealCursor()
    {
        if (_model is not { } model || model.CursorIndex is var index && index < 0)
        {
            return;
        }

        var visible = Math.Max(1, VisibleRowCount());
        if (index < FirstVisibleRow)
        {
            ScrollToRow(index);
        }
        else if (index >= FirstVisibleRow + visible)
        {
            ScrollToRow(index - visible + 1);
        }
    }

    private void Start(Func<CancellationToken, Task> work)
    {
        ArgumentNullException.ThrowIfNull(work);
        _ = RunAsync(work, _load.Token);
    }

    private async Task RunAsync(Func<CancellationToken, Task> work, CancellationToken token)
    {
        try
        {
            await work(token).ConfigureAwait(true);
            _error = null;
        }
        catch (OperationCanceledException)
        {
            return;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            _error = error.Message;
        }
        finally
        {
            InvalidateVisual();
        }
    }

    private async Task EnsureInitializedAsync(CancellationToken cancellationToken)
    {
        if (_model is not { } model || _initialized)
        {
            return;
        }

        _initialized = true;
        await model.InitializeAsync(cancellationToken).ConfigureAwait(true);
        RevealCursor();
        InvalidateMeasure();
    }

    private async Task LoadMoreForViewportAsync(CancellationToken cancellationToken)
    {
        if (_model is not { } model || VisibleRowCount() <= 0)
        {
            return;
        }

        await model.LoadMoreForWindowAsync(FirstVisibleRow, VisibleRowCount(), cancellationToken).ConfigureAwait(true);
    }

    internal void ApplyQuery(HierarchyQuery query)
    {
        if (_model is not { } model)
        {
            return;
        }

        _scrollRows = 0;
        Start(token => model.SetQueryAsync(query, token));
    }
}
