using Avalonia;
using Avalonia.Controls;

namespace Spotledger.Client.Forms;

/// <summary>
/// A responsive column flow: as many columns as fit the width (each at least
/// <see cref="MinColumnWidth"/> wide, at most <see cref="MaxColumns"/>), children placed in the
/// shortest column. At a narrow width the form collapses to one column; the field order stays
/// reading order within a row of the flow.
/// </summary>
public sealed class ColumnsPanel : Panel
{
    /// <summary>Defines <see cref="MinColumnWidth"/>.</summary>
    public static readonly StyledProperty<double> MinColumnWidthProperty =
        AvaloniaProperty.Register<ColumnsPanel, double>(nameof(MinColumnWidth), 320);

    /// <summary>Defines <see cref="Gap"/>.</summary>
    public static readonly StyledProperty<double> GapProperty =
        AvaloniaProperty.Register<ColumnsPanel, double>(nameof(Gap), 16);

    /// <summary>Defines <see cref="MaxColumns"/>.</summary>
    public static readonly StyledProperty<int> MaxColumnsProperty =
        AvaloniaProperty.Register<ColumnsPanel, int>(nameof(MaxColumns), 3);

    private int _columns = 1;
    private double[] _heights = [0];

    static ColumnsPanel()
    {
        AffectsMeasure<ColumnsPanel>(MinColumnWidthProperty, GapProperty, MaxColumnsProperty);
    }

    /// <summary>The narrowest a column may be before the flow drops a column.</summary>
    public double MinColumnWidth
    {
        get => GetValue(MinColumnWidthProperty);
        set => SetValue(MinColumnWidthProperty, value);
    }

    /// <summary>The space between columns and between rows.</summary>
    public double Gap
    {
        get => GetValue(GapProperty);
        set => SetValue(GapProperty, value);
    }

    /// <summary>The most columns used, however wide the panel is.</summary>
    public int MaxColumns
    {
        get => GetValue(MaxColumnsProperty);
        set => SetValue(MaxColumnsProperty, value);
    }

    /// <summary>The number of columns the last measure chose.</summary>
    public int ColumnCount => _columns;

    /// <summary>The number of columns that fit <paramref name="width"/>.</summary>
    public int ColumnsFor(double width)
    {
        if (double.IsInfinity(width) || width <= 0)
        {
            return 1;
        }

        return Math.Clamp((int)((width + Gap) / (MinColumnWidth + Gap)), 1, Math.Max(1, MaxColumns));
    }

    /// <inheritdoc />
    protected override Size MeasureOverride(Size availableSize)
    {
        _columns = ColumnsFor(availableSize.Width);
        var width = double.IsInfinity(availableSize.Width) ? MinColumnWidth : availableSize.Width;
        var columnWidth = (width - Gap * (_columns - 1)) / _columns;
        _heights = new double[_columns];
        foreach (var child in Children)
        {
            if (!child.IsVisible)
            {
                continue;
            }

            child.Measure(new Size(columnWidth, double.PositiveInfinity));
            var column = ShortestColumn();
            _heights[column] += child.DesiredSize.Height + (_heights[column] > 0 ? Gap : 0);
        }

        return new Size(width, _heights.Max());
    }

    /// <inheritdoc />
    protected override Size ArrangeOverride(Size finalSize)
    {
        _columns = ColumnsFor(finalSize.Width);
        var columnWidth = (finalSize.Width - Gap * (_columns - 1)) / _columns;
        var heights = new double[_columns];
        foreach (var child in Children)
        {
            if (!child.IsVisible)
            {
                continue;
            }

            var column = 0;
            for (var i = 1; i < heights.Length; i++)
            {
                if (heights[i] < heights[column])
                {
                    column = i;
                }
            }

            child.Arrange(new Rect(column * (columnWidth + Gap), heights[column], columnWidth, child.DesiredSize.Height));
            heights[column] += child.DesiredSize.Height + Gap;
        }

        return finalSize;
    }

    private int ShortestColumn()
    {
        var column = 0;
        for (var i = 1; i < _heights.Length; i++)
        {
            if (_heights[i] < _heights[column])
            {
                column = i;
            }
        }

        return column;
    }
}
