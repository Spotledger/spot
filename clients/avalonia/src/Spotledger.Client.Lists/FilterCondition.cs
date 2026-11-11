using System.Globalization;
using System.Text.RegularExpressions;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

/// <summary>The operators the filter builder offers.</summary>
public enum FilterOperator
{
    /// <summary>Text contains (case-insensitive).</summary>
    Contains,
    /// <summary>Equals.</summary>
    Equals,
    /// <summary>Does not equal.</summary>
    NotEquals,
    /// <summary>Text starts with (case-insensitive).</summary>
    StartsWith,
    /// <summary>Greater than or equal (range start).</summary>
    From,
    /// <summary>Less than or equal (range end).</summary>
    To,
    /// <summary>Between two values, both included.</summary>
    Between,
    /// <summary>One of several values (multi select, link by lookup).</summary>
    OneOf,
    /// <summary>Checkbox is set / not set (value true or false).</summary>
    Is,
    /// <summary>Has no value.</summary>
    IsEmpty,
    /// <summary>Has a value.</summary>
    IsNotEmpty,
}

/// <summary>One condition of the builder: field, operator and the entered values (text, as
/// typed or picked). Turning it into a <see cref="ListFilter"/> validates every value against
/// the field's kind; nothing typed is ever put into a query as text.</summary>
public sealed record FilterCondition(string Field, FilterOperator Operator, IReadOnlyList<string> Values)
{
    private static readonly Regex DecimalPattern = new(@"^-?[0-9]{1,38}(\.[0-9]{1,38})?$", RegexOptions.CultureInvariant, TimeSpan.FromSeconds(1));

    /// <summary>The operators offered for a field kind.</summary>
    public static IReadOnlyList<FilterOperator> OperatorsFor(FieldKind kind) => kind switch
    {
        FieldKind.Text => [FilterOperator.Contains, FilterOperator.Equals, FilterOperator.StartsWith, FilterOperator.NotEquals, FilterOperator.IsEmpty, FilterOperator.IsNotEmpty],
        FieldKind.IntegerNumber or FieldKind.DecimalNumber or FieldKind.Date or FieldKind.Datetime
            => [FilterOperator.Equals, FilterOperator.Between, FilterOperator.From, FilterOperator.To, FilterOperator.IsEmpty, FilterOperator.IsNotEmpty],
        FieldKind.Link => [FilterOperator.OneOf, FilterOperator.IsEmpty, FilterOperator.IsNotEmpty],
        FieldKind.Select => [FilterOperator.OneOf, FilterOperator.IsEmpty, FilterOperator.IsNotEmpty],
        FieldKind.Bool => [FilterOperator.Is],
        _ => [],
    };

    /// <summary>Whether the kind can be filtered at all.</summary>
    public static bool IsFilterable(FieldKind kind) => OperatorsFor(kind).Count > 0;

    /// <summary>How many values the operator needs.</summary>
    public static int ValueCount(FilterOperator op) => op switch
    {
        FilterOperator.IsEmpty or FilterOperator.IsNotEmpty => 0,
        FilterOperator.Between => 2,
        FilterOperator.OneOf => -1,
        _ => 1,
    };

    /// <summary>Builds the filter, or returns false with a reason naming what is wrong.</summary>
    public bool TryBuild(FieldModel field, out ListFilter? filter, out string? error)
    {
        filter = null;
        error = null;
        ArgumentNullException.ThrowIfNull(field);
        if (field.Name != Field || !ListLimits.IsIdentifier(field.Name))
        {
            error = "Unknown field.";
            return false;
        }

        if (!OperatorsFor(field.Kind).Contains(Operator))
        {
            error = $"{field.Label} cannot be filtered that way.";
            return false;
        }

        var need = ValueCount(Operator);
        if (need >= 0 && Values.Count != need || need < 0 && (Values.Count == 0 || Values.Count > ListLimits.MaxInValues))
        {
            error = "Enter a value.";
            return false;
        }

        switch (Operator)
        {
            case FilterOperator.IsEmpty:
                filter = new NullFilter(Field, true);
                return true;
            case FilterOperator.IsNotEmpty:
                filter = new NullFilter(Field, false);
                return true;
            case FilterOperator.Is:
                if (!bool.TryParse(Values.Count > 0 ? Values[0] : "true", out var flag))
                {
                    error = "Choose set or not set.";
                    return false;
                }

                filter = new CompareFilter(Field, CompareOp.Eq, ListScalar.Of(flag));
                return true;
            case FilterOperator.Contains:
            case FilterOperator.StartsWith:
                if (string.IsNullOrEmpty(Values[0]))
                {
                    error = "Enter a value.";
                    return false;
                }

                filter = new TextFilter(Field, Operator == FilterOperator.Contains ? TextOp.Contains : TextOp.Prefix, Values[0], true);
                return true;
            case FilterOperator.OneOf:
                var scalars = new List<ListScalar>();
                foreach (var value in Values)
                {
                    if (!TryScalar(field, value, out var scalar, out error))
                    {
                        return false;
                    }

                    scalars.Add(scalar);
                }

                filter = new InFilter(Field, scalars);
                return true;
            case FilterOperator.Between:
                if (!TryScalar(field, Values[0], out var low, out error) || !TryScalar(field, Values[1], out var high, out error))
                {
                    return false;
                }

                filter = new AndFilter([new CompareFilter(Field, CompareOp.Ge, low), new CompareFilter(Field, CompareOp.Le, high)]);
                return true;
            default:
                if (!TryScalar(field, Values[0], out var one, out error))
                {
                    return false;
                }

                var op = Operator switch
                {
                    FilterOperator.NotEquals => CompareOp.Ne,
                    FilterOperator.From => CompareOp.Ge,
                    FilterOperator.To => CompareOp.Le,
                    _ => CompareOp.Eq,
                };
                filter = new CompareFilter(Field, op, one);
                return true;
        }
    }

    /// <summary>Parses one entered value by the field's kind into a typed scalar.</summary>
    public static bool TryScalar(FieldModel field, string text, out ListScalar scalar, out string? error)
    {
        scalar = default;
        error = null;
        text = text?.Trim() ?? string.Empty;
        switch (field.Kind)
        {
            case FieldKind.IntegerNumber:
                if (long.TryParse(text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out var number))
                {
                    scalar = ListScalar.Of(number);
                    return true;
                }

                error = $"{field.Label} needs a whole number.";
                return false;
            case FieldKind.DecimalNumber:
                if (DecimalPattern.IsMatch(text))
                {
                    scalar = ListScalar.Of(text);
                    return true;
                }

                error = $"{field.Label} needs a number such as 12.50.";
                return false;
            case FieldKind.Date:
                if (TemporalFormat.TryParseDate(text, out var date))
                {
                    scalar = ListScalar.Of(TemporalFormat.WriteDate(date));
                    return true;
                }

                error = $"{field.Label} needs a date as YYYY-MM-DD.";
                return false;
            case FieldKind.Datetime:
                if (TemporalFormat.TryParseDateTime(text, out var moment))
                {
                    scalar = ListScalar.Of(TemporalFormat.WriteDateTime(moment));
                    return true;
                }

                if (TemporalFormat.TryParseDate(text, out var day))
                {
                    scalar = ListScalar.Of(TemporalFormat.WriteDate(day) + "T00:00:00Z");
                    return true;
                }

                error = $"{field.Label} needs a date and time.";
                return false;
            case FieldKind.Select:
                if (field.Options.Contains(text, StringComparer.Ordinal))
                {
                    scalar = ListScalar.Of(text);
                    return true;
                }

                error = $"{text} is not an option of {field.Label}.";
                return false;
            case FieldKind.Text or FieldKind.LongText or FieldKind.Link:
                if (text.Length == 0)
                {
                    error = "Enter a value.";
                    return false;
                }

                scalar = ListScalar.Of(text);
                return true;
            default:
                error = $"{field.Label} cannot be filtered.";
                return false;
        }
    }

    /// <summary>Builds the filter for a whole set of conditions (all must hold). Returns the
    /// first error, naming the offending condition; the filter is null when empty.</summary>
    public static bool TryBuildAll(DocTypeModel model, IEnumerable<FilterCondition> conditions, out ListFilter? filter, out string? error)
    {
        filter = null;
        error = null;
        var parts = new List<ListFilter>();
        foreach (var condition in conditions)
        {
            var field = model.Field(condition.Field);
            if (field is null)
            {
                error = "Unknown field.";
                return false;
            }

            if (!condition.TryBuild(field, out var one, out error))
            {
                return false;
            }

            parts.Add(one!);
        }

        filter = parts.Count switch { 0 => null, 1 => parts[0], _ => new AndFilter(parts) };
        return true;
    }

    /// <summary>A short human description ("Status is one of Open, Paid").</summary>
    public string Describe(FieldModel field)
    {
        var values = string.Join(", ", Values);
        return Operator switch
        {
            FilterOperator.Contains => $"{field.Label} contains {values}",
            FilterOperator.Equals => $"{field.Label} = {values}",
            FilterOperator.NotEquals => $"{field.Label} ≠ {values}",
            FilterOperator.StartsWith => $"{field.Label} starts with {values}",
            FilterOperator.From => $"{field.Label} from {values}",
            FilterOperator.To => $"{field.Label} to {values}",
            FilterOperator.Between => $"{field.Label} {(Values.Count > 0 ? Values[0] : string.Empty)} to {(Values.Count > 1 ? Values[1] : string.Empty)}",
            FilterOperator.OneOf => $"{field.Label} in {values}",
            FilterOperator.Is => $"{field.Label} {(Values.Count > 0 && Values[0] == "false" ? "not set" : "set")}",
            FilterOperator.IsEmpty => $"{field.Label} is empty",
            _ => $"{field.Label} has a value",
        };
    }
}
