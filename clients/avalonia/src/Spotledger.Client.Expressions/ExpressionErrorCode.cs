using System;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>
/// The stable <c>expr.*</c> error codes of the expression language (specification section 9).
/// The codes are the contract; messages are not.
/// </summary>
public static class ExpressionErrorCode
{
    /// <summary>The text does not match the grammar, or an unexpected character.</summary>
    public const string Syntax = "expr.syntax";

    /// <summary>A text literal is not closed.</summary>
    public const string UnterminatedString = "expr.unterminated_string";

    /// <summary>An unknown escape in a text literal.</summary>
    public const string BadEscape = "expr.bad_escape";

    /// <summary>A numeric literal does not fit its type.</summary>
    public const string NumberRange = "expr.number_range";

    /// <summary>More than the maximum number of nodes.</summary>
    public const string TooComplex = "expr.too_complex";

    /// <summary>Nesting deeper than the maximum depth.</summary>
    public const string TooDeep = "expr.too_deep";

    /// <summary>An identifier that is not a keyword, call or path root.</summary>
    public const string UnknownName = "expr.unknown_name";

    /// <summary>A call to a function not in the library.</summary>
    public const string UnknownFunction = "expr.unknown_function";

    /// <summary>A call with an unacceptable number of arguments.</summary>
    public const string Arity = "expr.arity";

    /// <summary>An operand or argument of the wrong type, or an invalid argument value.</summary>
    public const string Type = "expr.type";

    /// <summary>Division or remainder by zero.</summary>
    public const string DivisionByZero = "expr.division_by_zero";

    /// <summary>An int, decimal or date result out of range.</summary>
    public const string Overflow = "expr.overflow";

    /// <summary>Text that is not a date or datetime.</summary>
    public const string BadDate = "expr.bad_date";

    /// <summary>Text that is not an integer or decimal.</summary>
    public const string BadNumber = "expr.bad_number";
}