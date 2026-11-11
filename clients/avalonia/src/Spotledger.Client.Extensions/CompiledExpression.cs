using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>
/// An expression of the shared language, compiled once. A string that does not parse keeps
/// its parser error and evaluates to the caller's fallback (hidden / disabled), so a broken
/// expression never becomes an exception on a render path.
/// </summary>
public sealed class CompiledExpression
{
    private CompiledExpression(string source, ExpressionNode? node, ExpressionParseException? parseError)
    {
        Source = source;
        Node = node;
        ParseError = parseError;
    }

    /// <summary>The expression text as the document carried it.</summary>
    public string Source { get; }

    /// <summary>The parsed tree, or null when the source does not parse.</summary>
    public ExpressionNode? Node { get; }

    /// <summary>The parser's error, or null when the source parses.</summary>
    public ExpressionParseException? ParseError { get; }

    /// <summary>True when the source parsed.</summary>
    public bool IsValid => Node is not null;

    /// <summary>Compiles a source string, never throwing.</summary>
    public static CompiledExpression Compile(string source)
    {
        ArgumentNullException.ThrowIfNull(source);
        try
        {
            return new CompiledExpression(source, Expression.Parse(source), null);
        }
        catch (ExpressionParseException error)
        {
            return new CompiledExpression(source, null, error);
        }
        catch (ArgumentException error)
        {
            return new CompiledExpression(source, null, new ExpressionParseException(ExpressionErrorCode.Syntax, error.Message, 0));
        }
    }

    /// <summary>
    /// Evaluates against a context. Any failure — the parse error, a type error, a division
    /// by zero — is reported once through <paramref name="sink" /> and yields
    /// <see cref="ExpressionValue.Null" />; this method never throws.
    /// </summary>
    /// <param name="context">The values the expression reads.</param>
    /// <param name="sink">Where a failure is reported (once per owner and code).</param>
    /// <param name="owner">The item id the expression belongs to.</param>
    public ExpressionValue Evaluate(IExpressionContext context, DiagnosticSink sink, string owner)
    {
        ArgumentNullException.ThrowIfNull(context);
        ArgumentNullException.ThrowIfNull(sink);
        ArgumentNullException.ThrowIfNull(owner);
        if (Node is null)
        {
            ReportParse(sink, owner);
            return ExpressionValue.Null;
        }
        try
        {
            return Expression.Evaluate(Node, context);
        }
        catch (ExpressionEvaluationException error)
        {
            sink.ReportOnce(owner, ExtensionCodes.ExpressionFailed, FormatFailure(error.Code, error.Message));
            return ExpressionValue.Null;
        }
        catch (Exception error) when (error is not OutOfMemoryException)
        {
            sink.ReportOnce(owner, ExtensionCodes.ExpressionFailed, FormatFailure(ExpressionErrorCode.Syntax, error.Message));
            return ExpressionValue.Null;
        }
    }

    /// <summary>
    /// Evaluates as a condition: a bool is itself, null is false, anything else is a type
    /// failure reported once and treated as <paramref name="fallback" />.
    /// </summary>
    public bool EvaluateBool(IExpressionContext context, DiagnosticSink sink, string owner, bool fallback)
    {
        ExpressionValue value = Evaluate(context, sink, owner);
        return value switch
        {
            BoolValue boolean => boolean.Value,
            NullValue when Node is null => fallback,
            NullValue => false,
            _ => ReportNotBool(value, sink, owner, fallback)
        };
    }

    /// <summary>Reports the parse error (once) for a broken expression.</summary>
    public void ReportParseError(DiagnosticSink sink, string owner)
    {
        ArgumentNullException.ThrowIfNull(sink);
        ArgumentNullException.ThrowIfNull(owner);
        if (Node is null)
        {
            ReportParse(sink, owner);
        }
    }

    private void ReportParse(DiagnosticSink sink, string owner)
    {
        ExpressionParseException? error = ParseError;
        string message = error is null
            ? "the expression does not parse"
            : $"{error.MessageText} at offset {error.Offset.ToString(System.Globalization.CultureInfo.InvariantCulture)}";
        sink.ReportOnce(owner, ExtensionCodes.ExpressionInvalid, message);
    }

    private static bool ReportNotBool(ExpressionValue value, DiagnosticSink sink, string owner, bool fallback)
    {
        sink.ReportOnce(owner, ExtensionCodes.ExpressionFailed, FormatFailure(ExpressionErrorCode.Type, $"the condition must be a bool, not {value.TypeName}"));
        return fallback;
    }

    private static string FormatFailure(string code, string message) => $"{code}: {message}";

    /// <inheritdoc/>
    public override string ToString() => Source;
}
