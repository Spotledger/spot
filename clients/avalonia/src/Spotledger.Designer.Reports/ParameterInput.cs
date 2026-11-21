using System.Globalization;

namespace Spotledger.Designer.Reports;

/// <summary>
/// One value a person types for a report parameter or a shared dashboard filter: the label, the
/// typed text, and the parse into an exact <see cref="FilterValue"/> (decimals stay text).
/// </summary>
public sealed class ParameterInput : ObservableObject
{
    private string _text = string.Empty;
    private string _error = string.Empty;

    /// <summary>Creates an input.</summary>
    public ParameterInput(string name, string label, ParamType type, FilterValue? initial = null)
    {
        Name = name;
        Label = label;
        Type = type;
        _text = initial switch
        {
            FilterValue.Text t => t.Value,
            FilterValue.Whole w => w.Value.ToString(CultureInfo.InvariantCulture),
            FilterValue.Exact e => e.Value,
            FilterValue.Day d => d.Value,
            FilterValue.Bool b => b.Value ? "yes" : "no",
            _ => string.Empty,
        };
    }

    /// <summary>The parameter or filter name.</summary>
    public string Name { get; }

    /// <summary>The label the person sees.</summary>
    public string Label { get; }

    /// <summary>The value type.</summary>
    public ParamType Type { get; }

    /// <summary>The typed text.</summary>
    public string Text
    {
        get => _text;
        set => SetField(ref _text, value ?? string.Empty);
    }

    /// <summary>What is wrong with the text, in plain words, or empty.</summary>
    public string Error
    {
        get => _error;
        private set => SetField(ref _error, value);
    }

    /// <summary>Parses the text; empty text is "no value" (true with a null value).</summary>
    public bool TryParse(out FilterValue? value)
    {
        value = null;
        Error = string.Empty;
        var trimmed = Text.Trim();
        if (trimmed.Length == 0)
        {
            return true;
        }

        switch (Type)
        {
            case ParamType.Whole:
                if (long.TryParse(trimmed, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var whole))
                {
                    value = new FilterValue.Whole(whole);
                    return true;
                }

                Error = $"{Label}: type a whole number, like 12.";
                return false;
            case ParamType.Exact:
                if (FilterEditor.IsExactDecimal(trimmed))
                {
                    value = new FilterValue.Exact(trimmed);
                    return true;
                }

                Error = $"{Label}: type an exact number, like 12.50.";
                return false;
            case ParamType.Day:
                if (DateOnly.TryParseExact(trimmed, "yyyy-MM-dd", CultureInfo.InvariantCulture, DateTimeStyles.None, out _))
                {
                    value = new FilterValue.Day(trimmed);
                    return true;
                }

                Error = $"{Label}: type a date as year-month-day, like 2026-03-31.";
                return false;
            default:
                value = new FilterValue.Text(trimmed);
                return true;
        }
    }

    /// <summary>Parses a set of inputs into a name-to-value map; stops at the first bad one and returns its error.</summary>
    public static bool TryCollect(IEnumerable<ParameterInput> inputs, out Dictionary<string, FilterValue> values, out string error)
    {
        ArgumentNullException.ThrowIfNull(inputs);
        values = new Dictionary<string, FilterValue>(StringComparer.Ordinal);
        error = string.Empty;
        foreach (var input in inputs)
        {
            if (!input.TryParse(out var value))
            {
                error = input.Error;
                return false;
            }

            if (value is not null)
            {
                values[input.Name] = value;
            }
        }

        return true;
    }
}
