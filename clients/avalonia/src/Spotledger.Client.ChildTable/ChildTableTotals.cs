using System.Numerics;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using RowDraft = Spotledger.Client.Metadata.DocumentDraft.TableDraft.RowDraft;

namespace Spotledger.Client.ChildTable;

/// <summary>The totals row: per declared column a sum, a count or an average, in exact
/// decimal arithmetic (BigInteger over the digits; no floating point, no System.Decimal
/// overflow on the 38-digit fields). A cell edit or a row change updates the one column
/// and row it touches; a bulk change recomputes.</summary>
public sealed class ChildTableTotals
{
    private readonly IReadOnlyList<ChildTableColumnInfo> _columns;
    private readonly ColumnState?[] _states;

    internal ChildTableTotals(IReadOnlyList<ChildTableColumnInfo> columns)
    {
        _columns = columns;
        _states = new ColumnState?[columns.Count];
        for (var i = 0; i < columns.Count; i++)
        {
            if (columns[i].Spec.Total is { } kind)
            {
                _states[i] = new ColumnState(columns[i].Field, kind);
            }
        }
    }

    /// <summary>True when any column declares a total.</summary>
    public bool HasTotals => _states.Any(state => state is not null);

    /// <summary>How many row contributions were recomputed since creation (tests assert an
    /// edit does not recompute the table).</summary>
    public int ContributionsComputed { get; private set; }

    /// <summary>The display text of a column's total, or an empty string when it declares none.</summary>
    public string TextFor(int column) => _states[column]?.Text() ?? string.Empty;

    /// <summary>Recomputes every declared column from the rows.</summary>
    internal void Recompute(IReadOnlyList<RowDraft> rows)
    {
        foreach (var state in _states)
        {
            state?.Clear();
        }

        foreach (var row in rows)
        {
            AddRow(row);
        }
    }

    /// <summary>Folds one row in.</summary>
    internal void AddRow(RowDraft row)
    {
        for (var i = 0; i < _states.Length; i++)
        {
            if (_states[i] is { } state)
            {
                state.Set(row, row.GetValue(_columns[i].Field.Name));
                ContributionsComputed++;
            }
        }
    }

    /// <summary>Takes one row out.</summary>
    internal void RemoveRow(RowDraft row)
    {
        foreach (var state in _states)
        {
            state?.Remove(row);
        }
    }

    /// <summary>Re-reads one cell.</summary>
    internal void CellChanged(RowDraft row, string field)
    {
        for (var i = 0; i < _states.Length; i++)
        {
            if (_states[i] is { } state && string.Equals(_columns[i].Field.Name, field, StringComparison.Ordinal))
            {
                state.Set(row, row.GetValue(field));
                ContributionsComputed++;
            }
        }
    }

    /// <summary>True when the column declares the field (a cell edit there changes the totals row).</summary>
    internal bool Affects(string field)
        => _columns.Where((column, i) => _states[i] is not null && column.Field.Name == field).Any();

    /// <summary>Builds the grid's footer row: the label in the first column unless that column
    /// has a total of its own.</summary>
    internal GridRow BuildFooter(string label)
    {
        var cells = new List<GridCell>(_columns.Count);
        for (var i = 0; i < _columns.Count; i++)
        {
            var text = TextFor(i);
            if (i == 0 && _states[0] is null)
            {
                text = label;
            }

            cells.Add(new GridCell(text, CellFormatter.Align(_columns[i].Field)));
        }

        return new GridRow("totals", cells);
    }

    private readonly record struct Num(BigInteger Unscaled, int Scale);

    private sealed class ColumnState(FieldModel field, ChildTableTotal kind)
    {
        private readonly Dictionary<RowDraft, Num?> _values = new(ReferenceEqualityComparer.Instance);
        private BigInteger _sum;
        private int _scale = field.Scale ?? 0;
        private int _count;

        public void Clear()
        {
            _values.Clear();
            _sum = BigInteger.Zero;
            _scale = field.Scale ?? 0;
            _count = 0;
        }

        public void Remove(RowDraft row)
        {
            if (_values.Remove(row, out var old) && old is { } number)
            {
                if (kind != ChildTableTotal.Count)
                {
                    _sum -= Rescale(number, _scale);
                }

                _count--;
            }
        }

        public void Set(RowDraft row, FieldValue? value)
        {
            Remove(row);
            if (value is null || value.IsNull)
            {
                _values[row] = null;
                return;
            }

            if (kind == ChildTableTotal.Count)
            {
                _values[row] = new Num(BigInteger.One, 0);
                _count++;
                return;
            }

            var number = Parse(value);
            if (number is null)
            {
                _values[row] = null;
                return;
            }

            var parsed = number.Value;
            if (parsed.Scale > _scale)
            {
                // A value with more fraction digits than the column has seen: widen the
                // working scale once, rescaling the sum, so no digit is ever dropped.
                _sum *= BigInteger.Pow(10, parsed.Scale - _scale);
                _scale = parsed.Scale;
            }

            _values[row] = parsed;
            _sum += Rescale(parsed, _scale);
            _count++;
        }

        public string Text()
        {
            switch (kind)
            {
                case ChildTableTotal.Count:
                    return _count.ToString(System.Globalization.CultureInfo.InvariantCulture);
                case ChildTableTotal.Sum:
                    return Render(_sum, _scale, field.Scale ?? 0);
                default:
                    if (_count == 0)
                    {
                        return string.Empty;
                    }

                    // Average: divide at the working scale, rounding half away from zero at
                    // the field's scale (display only).
                    var target = Math.Max(field.Scale ?? 0, 0);
                    var scaled = _scale >= target
                        ? _sum
                        : _sum * BigInteger.Pow(10, target - _scale);
                    var scaleUsed = Math.Max(_scale, target);
                    var quotient = BigInteger.DivRem(scaled, _count, out var remainder);
                    if (BigInteger.Abs(remainder) * 2 >= _count)
                    {
                        quotient += scaled.Sign < 0 ? BigInteger.MinusOne : BigInteger.One;
                    }

                    return Render(quotient, scaleUsed, field.Scale ?? 0);
            }
        }

        private static BigInteger Rescale(Num number, int scale)
            => scale == number.Scale ? number.Unscaled : number.Unscaled * BigInteger.Pow(10, scale - number.Scale);

        private static Num? Parse(FieldValue value)
        {
            string text;
            switch (value)
            {
                case DecimalFieldValue decimalValue:
                    text = decimalValue.CanonicalText;
                    break;
                case IntFieldValue integer:
                    return new Num(integer.Value, 0);
                default:
                    return null;
            }

            var negative = text.StartsWith('-');
            var body = negative ? text[1..] : text;
            var dot = body.IndexOf('.', StringComparison.Ordinal);
            var digits = dot < 0 ? body : body.Remove(dot, 1);
            var scale = dot < 0 ? 0 : body.Length - dot - 1;
            if (!BigInteger.TryParse(digits, System.Globalization.NumberStyles.None, System.Globalization.CultureInfo.InvariantCulture, out var unscaled))
            {
                return null;
            }

            return new Num(negative ? -unscaled : unscaled, scale);
        }

        private static string Render(BigInteger unscaled, int scale, int minScale)
        {
            var negative = unscaled.Sign < 0;
            var digits = BigInteger.Abs(unscaled).ToString(System.Globalization.CultureInfo.InvariantCulture);
            if (scale > 0)
            {
                digits = digits.PadLeft(scale + 1, '0');
                digits = digits[..^scale] + "." + digits[^scale..];
            }

            var text = CellFormatter.PadScale(digits, minScale);
            return negative && unscaled.Sign != 0 ? "-" + text : text;
        }
    }
}
