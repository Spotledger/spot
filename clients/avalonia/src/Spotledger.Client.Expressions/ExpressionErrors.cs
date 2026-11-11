using System;

namespace Spotledger.Client.Expressions;

/// <summary>
/// A problem found while parsing: the code, a message and a character offset (0-based,
/// counting Unicode scalar values, of the offending token's first character).
/// </summary>
public sealed class ExpressionParseException : Exception
{
    /// <summary>Creates a parse error.</summary>
    public ExpressionParseException(string code, string message, int offset)
        : base(code + " at " + offset.ToString(provider: null) + ": " + message)
    {
        Code = code;
        MessageText = message;
        Offset = offset;
    }

    /// <summary>The stable error code.</summary>
    public string Code { get; }

    /// <summary>What is wrong, in a sentence.</summary>
    public string MessageText { get; }

    /// <summary>The 0-based character offset of the offending token.</summary>
    public int Offset { get; }
}

/// <summary>
/// A problem found while evaluating: the code and a message (no position).
/// </summary>
public sealed class ExpressionEvaluationException : Exception
{
    /// <summary>Creates an evaluation error.</summary>
    public ExpressionEvaluationException(string code, string message)
        : base(code + ": " + message)
    {
        Code = code;
        MessageText = message;
    }

    /// <summary>The stable error code.</summary>
    public string Code { get; }

    /// <summary>What is wrong, in a sentence.</summary>
    public string MessageText { get; }
}