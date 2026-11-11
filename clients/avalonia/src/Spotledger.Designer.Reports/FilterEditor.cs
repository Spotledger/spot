using System.Globalization;

namespace Spotledger.Designer.Reports;

/// <summary>The kind of editor a filter row shows for a field's type.</summary>
public enum EditorKind
{
    /// <summary>A text box.</summary>
    Text,

    /// <summary>A whole-number box.</summary>
    Whole,

    /// <summary>An exact-decimal box.</summary>
    Exact,

    /// <summary>A date box (YYYY-MM-DD).</summary>
    Date,

    /// <summary>A yes/no switch.</summary>
    YesNo,
}

/// <summary>
/// One filter being built: a typed editor for the field's type, the comparison it allows, and the
/// "let the viewer choose" parameter toggle. It only checks that the text can be the field's type
/// (so a bad number is caught at the keyboard); the server validates the filter itself.
/// </summary>
public sealed class FilterEditor : Spotledger.Client.Designer.ObservableObject
{
    private Compare _operator = Compare.Eq;
    private string _valueText = string.Empty;
    private bool _yesNo = true;
    private bool _isParameter;
    private string _error = string.Empty;

    /// <summary>Creates the editor for a field.</summary>
    public FilterEditor(FieldChoice field)
    {
        Field = field ?? throw new ArgumentNullException(nameof(field));
        Editor = field.Offer.Kind switch
        {
            OfferKind.Whole => EditorKind.Whole,
            OfferKind.Exact => EditorKind.Exact,
            OfferKind.Day => EditorKind.Date,
            OfferKind.Bool => EditorKind.YesNo,
            _ => EditorKind.Text,
        };
        Operators = Editor switch
        {
            EditorKind.Whole or EditorKind.Exact or EditorKind.Date => new[] { Compare.Eq, Compare.Ne, Compare.Lt, Compare.Le, Compare.Gt, Compare.Ge },
            EditorKind.YesNo => new[] { Compare.Eq },
            _ => new[] { Compare.Eq, Compare.Ne },
        };
    }

    /// <summary>The field being filtered.</summary>
    public FieldChoice Field { get; }

    /// <summary>The editor the field's type calls for.</summary>
    public EditorKind Editor { get; }

    /// <summary>The comparisons that make sense for this type.</summary>
    public IReadOnlyList<Compare> Operators { get; }

    /// <summary>Whether the viewer can be asked for the value instead (not for yes/no).</summary>
    public bool CanBeParameter => Editor != EditorKind.YesNo;

    /// <summary>The chosen comparison.</summary>
    public Compare Operator
    {
        get => _operator;
        set
        {
            if (!Operators.Contains(value))
            {
                throw new ArgumentOutOfRangeException(nameof(value), "That comparison does not fit this field.");
            }

            SetField(ref _operator, value);
        }
    }

    /// <summary>The typed value (a default when this is a parameter).</summary>
    public string ValueText
    {
        get => _valueText;
        set => SetField(ref _valueText, value ?? string.Empty);
    }

    /// <summary>The yes/no value.</summary>
    public bool YesNo
    {
        get => _yesNo;
        set => SetField(ref _yesNo, value);
    }

    /// <summary>Whether the viewer supplies the value when running the report.</summary>
    public bool IsParameter
    {
        get => _isParameter;
        set
        {
            if (value && !CanBeParameter)
            {
                return;
            }

            SetField(ref _isParameter, value);
        }
    }

    /// <summary>The last problem found by <see cref="TryBuild"/>, in plain words.</summary>
    public string Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>The parameter name this filter would use.</summary>
    public string ParameterName => Field.Offer.Path.Replace('.', '_').ToLowerInvariant();

    /// <summary>Parses the typed value as the field's type.</summary>
    public bool TryParseValue(string text, out FilterValue? value, out string error)
    {
        value = null;
        error = string.Empty;
        var trimmed = (text ?? string.Empty).Trim();
        switch (Editor)
        {
            case EditorKind.YesNo:
                value = new FilterValue.Bool(_yesNo);
                return true;
            case EditorKind.Whole:
                if (long.TryParse(trimmed, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var whole))
                {
                    value = new FilterValue.Whole(whole);
                    return true;
                }

                error = "Type a whole number, like 12.";
                return false;
            case EditorKind.Exact:
                if (IsExactDecimal(trimmed))
                {
                    value = new FilterValue.Exact(trimmed);
                    return true;
                }

                error = "Type an exact number, like 12.50.";
                return false;
            case EditorKind.Date:
                if (DateOnly.TryParseExact(trimmed, "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out _))
                {
                    value = new FilterValue.Day(trimmed);
                    return true;
                }

                error = "Type a date as year-month-day, like 2026-03-31.";
                return false;
            default:
                if (trimmed.Length == 0)
                {
                    error = "Type the value to compare with.";
                    return false;
                }

                value = new FilterValue.Text(trimmed);
                return true;
        }
    }

    /// <summary>
    /// Builds the ops that add this filter: a parameter first when the viewer chooses the value,
    /// then the filter. Returns false with <see cref="Error"/> set when the value is not valid.
    /// </summary>
    public bool TryBuild(out IReadOnlyList<ReportOp> ops)
    {
        ops = Array.Empty<ReportOp>();
        FilterValue? value = null;
        var hasValue = !(IsParameter && string.IsNullOrWhiteSpace(ValueText) && Editor != EditorKind.YesNo);
        if (hasValue && !TryParseValue(ValueText, out value, out var problem))
        {
            Error = problem;
            return false;
        }

        Error = string.Empty;
        var path = Field.Offer.Path;
        if (!IsParameter)
        {
            ops = new ReportOp[] { new ReportOp.AddFilter(new DraftFilter(path, Operator, new FilterTarget.Literal(value!))) };
            return true;
        }

        var type = Field.Offer.Kind switch
        {
            OfferKind.Whole => ParamType.Whole,
            OfferKind.Exact => ParamType.Exact,
            OfferKind.Day => ParamType.Day,
            OfferKind.Link => ParamType.Link,
            _ => ParamType.Text,
        };
        var parameter = new DraftParameter(ParameterName, Field.Offer.Label, type, Required: false, Default: value);
        ops = new ReportOp[]
        {
            new ReportOp.AddParameter(parameter),
            new ReportOp.AddFilter(new DraftFilter(path, Operator, new FilterTarget.Parameter(ParameterName))),
        };
        return true;
    }

    /// <summary>Whether the text is a plain exact decimal: optional sign, digits, at most one point.</summary>
    public static bool IsExactDecimal(string text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return false;
        }

        var i = text[0] is '+' or '-' ? 1 : 0;
        var digitsBefore = 0;
        while (i < text.Length && char.IsAsciiDigit(text[i]))
        {
            i++;
            digitsBefore++;
        }

        if (digitsBefore == 0)
        {
            return false;
        }

        if (i == text.Length)
        {
            return true;
        }

        if (text[i] != '.')
        {
            return false;
        }

        i++;
        var digitsAfter = 0;
        while (i < text.Length && char.IsAsciiDigit(text[i]))
        {
            i++;
            digitsAfter++;
        }

        return digitsAfter > 0 && i == text.Length;
    }
}
