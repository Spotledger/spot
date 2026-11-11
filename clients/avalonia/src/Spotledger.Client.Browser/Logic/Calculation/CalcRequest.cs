using Spotledger.Client.Expressions;

namespace Spotledger.Client.Browser;

/// <summary>
/// One calculation preview: an expression of the shared expression language (a total, a tax amount, a
/// price rule) with the field values it reads and the instant <c>now()</c> returns. Time is an
/// argument: the caller supplies <see cref="Now"/>, the path never reads a clock.
/// </summary>
/// <param name="Source">The expression text.</param>
/// <param name="Now">The instant <c>now()</c> returns.</param>
/// <param name="Context">Field values by dotted path (<c>doc.qty</c>, <c>row.rate</c>); an absent path reads as null.</param>
public sealed record CalcRequest(string Source, ExpressionDateTime Now, IReadOnlyDictionary<string, ExpressionValue> Context)
{
    /// <summary>Builds the evaluation context of this request.</summary>
    public ExpressionMapContext ToContext()
    {
        var context = new ExpressionMapContext(Now);
        foreach (var (path, value) in Context.OrderBy(pair => pair.Key, StringComparer.Ordinal))
        {
            context = context.With(path, value);
        }

        return context;
    }
}

/// <summary>
/// The result of a calculation preview: a value, or the code (and for a syntax error the offset) of
/// the error. The two backends must give the same outcome for the same request.
/// </summary>
public sealed class CalcOutcome
{
    private CalcOutcome(ExpressionValue? value, string? errorCode, int? offset)
    {
        Value = value;
        ErrorCode = errorCode;
        Offset = offset;
    }

    /// <summary>The value, or null when the request failed.</summary>
    public ExpressionValue? Value { get; }

    /// <summary>The error code, or null on success.</summary>
    public string? ErrorCode { get; }

    /// <summary>The 0-based character offset of a syntax error, or null.</summary>
    public int? Offset { get; }

    /// <summary>True when the request produced a value.</summary>
    public bool IsOk => Value is not null;

    /// <summary>A successful outcome.</summary>
    public static CalcOutcome Ok(ExpressionValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return new CalcOutcome(value, null, null);
    }

    /// <summary>A failed outcome.</summary>
    public static CalcOutcome Error(string code, int? offset = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(code);
        return new CalcOutcome(null, code, offset);
    }

    /// <summary>Same type, value and (for decimals) scale, or same error code and offset.</summary>
    public bool Same(CalcOutcome other)
    {
        ArgumentNullException.ThrowIfNull(other);
        if (IsOk != other.IsOk)
        {
            return false;
        }

        return IsOk
            ? Value!.Same(other.Value!)
            : string.Equals(ErrorCode, other.ErrorCode, StringComparison.Ordinal) && Offset == other.Offset;
    }

    /// <inheritdoc />
    public override string ToString() => IsOk ? Value!.ToString() : $"error {ErrorCode}" + (Offset is { } o ? $" at {o}" : string.Empty);
}
