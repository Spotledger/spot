using System;
using System.Collections.Generic;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>
/// The facade of the language: parse, evaluate, print. Pure and total; the only outside reads
/// are the context's fields and <c>now</c>.
/// </summary>
public static class Expression
{
    /// <summary>The most nodes an expression may have.</summary>
    public const int MaxNodes = ExpressionParser.MaxNodes;

    /// <summary>The deepest nesting an expression may have.</summary>
    public const int MaxDepth = ExpressionParser.MaxDepth;

    /// <summary>Parses an expression, or throws <see cref="ExpressionParseException" />
    /// with the stable code and the character offset of the offending token.</summary>
    public static ExpressionNode Parse(string source) => ExpressionParser.Parse(source);

    /// <summary>Evaluates a parsed expression against a context, or throws
    /// <see cref="ExpressionEvaluationException" /> with the stable code.</summary>
    public static ExpressionValue Evaluate(ExpressionNode node, IExpressionContext context) =>
        ExpressionEvaluator.Evaluate(node, context);

    /// <summary>Parses and evaluates in one step.</summary>
    public static ExpressionValue Evaluate(string source, IExpressionContext context) =>
        Evaluate(Parse(source), context);
}

/// <summary>Evaluation (specification sections 5, 6 and 9): mirrors the Rust evaluator,
/// including the order operands are evaluated in and which error is reported first.</summary>
internal static class ExpressionEvaluator
{
    internal static ExpressionValue Evaluate(ExpressionNode node, IExpressionContext ctx)
    {
        switch (node)
        {
            case NullNode:
                return ExpressionValue.Null;
            case BoolNode b:
                return new BoolValue(b.Value);
            case IntNode n:
                return new IntValue(n.Value);
            case DecimalNode d:
                return new DecimalValue(d.Mantissa, d.Scale);
            case TextNode t:
                return new TextValue(t.Value);
            case ListNode list:
                {
                    ExpressionValue[] items = new ExpressionValue[list.Items.Count];
                    for (int i = 0; i < list.Items.Count; i++)
                    {
                        items[i] = Evaluate(list.Items[i], ctx);
                    }
                    return new ListValue(items);
                }
            case PathNode path:
                return ctx.Field(path.Root, path.Names) ?? ExpressionValue.Null;
            case UnaryNode unary:
                {
                    ExpressionValue v = Evaluate(unary.Inner, ctx);
                    return unary.Op == ExpressionUnaryOp.Not
                        ? new BoolValue(!Truth(v, "the operand of `not`"))
                        : Negate(v);
                }
            case BinaryNode binary:
                return Binary(binary, ctx);
            case TernaryNode ternary:
                {
                    bool condition = Truth(Evaluate(ternary.Condition, ctx), "the condition of `?`");
                    return Evaluate(condition ? ternary.WhenTrue : ternary.WhenFalse, ctx);
                }
            case CallNode call:
                {
                    ExpressionValue[] values = new ExpressionValue[call.Arguments.Count];
                    for (int i = 0; i < call.Arguments.Count; i++)
                    {
                        values[i] = Evaluate(call.Arguments[i], ctx);
                    }
                    return ExpressionFunctions.Call(call.Name, values, ctx.Now);
                }
            default:
                throw new ExpressionEvaluationException(ExpressionErrorCode.Syntax, "unknown node");
        }
    }

    /// <summary>The truth of a condition operand: null is false; anything but a bool is a type error.</summary>
    private static bool Truth(ExpressionValue v, string what) => v switch
    {
        BoolValue b => b.Value,
        NullValue => false,
        _ => throw TypeError($"{what} must be a bool, not {v.TypeName}"),
    };

    private static ExpressionEvaluationException TypeError(string message) =>
        new(ExpressionErrorCode.Type, message);

    private static ExpressionEvaluationException IntOverflow() =>
        new(ExpressionErrorCode.Overflow, "the integer is out of range");

    private static ExpressionEvaluationException DivisionByZero() =>
        new(ExpressionErrorCode.DivisionByZero, "division by zero");

    private static ExpressionValue Negate(ExpressionValue v) => v switch
    {
        NullValue => ExpressionValue.Null,
        IntValue n => NegateInt(n.Value),
        DecimalValue d => DecimalFrom(ExpressionDecimal.Make(-d.Mantissa, d.Scale)),
        _ => throw TypeError($"cannot negate {v.TypeName}"),
    };

    private static IntValue NegateInt(long n)
    {
        try
        {
            return new IntValue(checked(-n));
        }
        catch (OverflowException)
        {
            throw IntOverflow();
        }
    }

    internal static ExpressionValue DecimalFrom(ExpressionDecimal? d) =>
        d is null
            ? throw new ExpressionEvaluationException(ExpressionErrorCode.Overflow, "the decimal result is out of range")
            : new DecimalValue(d.Value.Mantissa, d.Value.Scale);

    /// <summary><c>==</c> of the language: numbers by value across int and decimal, lists
    /// elementwise, everything else by type and value; null equals only null.</summary>
    internal static bool ValuesEqual(ExpressionValue a, ExpressionValue b)
    {
        if (a is ListValue x && b is ListValue y)
        {
            if (x.Values.Count != y.Values.Count)
            {
                return false;
            }
            for (int i = 0; i < x.Values.Count; i++)
            {
                if (!ValuesEqual(x.Values[i], y.Values[i]))
                {
                    return false;
                }
            }
            return true;
        }
        ExpressionDecimal? da = AsDecimal(a);
        ExpressionDecimal? db = AsDecimal(b);
        if (da is not null && db is not null)
        {
            return da.Value.NumericEquals(db.Value);
        }
        if (da is null && db is null)
        {
            return a.Equals(b);
        }
        return false;
    }

    /// <summary>A decimal view of a numeric value.</summary>
    internal static ExpressionDecimal? AsDecimal(ExpressionValue v) => v switch
    {
        IntValue n => new ExpressionDecimal((Int128)n.Value, 0),
        DecimalValue d => d.ToDecimal(),
        _ => null,
    };

    private static ExpressionValue Binary(BinaryNode node, IExpressionContext ctx)
    {
        switch (node.Op)
        {
            case ExpressionBinaryOp.And:
                {
                    if (!Truth(Evaluate(node.Left, ctx), "the operand of `and`"))
                    {
                        return new BoolValue(false);
                    }
                    return new BoolValue(Truth(Evaluate(node.Right, ctx), "the operand of `and`"));
                }
            case ExpressionBinaryOp.Or:
                {
                    if (Truth(Evaluate(node.Left, ctx), "the operand of `or`"))
                    {
                        return new BoolValue(true);
                    }
                    return new BoolValue(Truth(Evaluate(node.Right, ctx), "the operand of `or`"));
                }
            default:
                break;
        }
        ExpressionValue a = Evaluate(node.Left, ctx);
        ExpressionValue b = Evaluate(node.Right, ctx);
        switch (node.Op)
        {
            case ExpressionBinaryOp.Eq:
                return new BoolValue(ValuesEqual(a, b));
            case ExpressionBinaryOp.Ne:
                return new BoolValue(!ValuesEqual(a, b));
            case ExpressionBinaryOp.Lt:
            case ExpressionBinaryOp.Le:
            case ExpressionBinaryOp.Gt:
            case ExpressionBinaryOp.Ge:
                return Ordering(node.Op, a, b);
            case ExpressionBinaryOp.In:
                return b switch
                {
                    NullValue => new BoolValue(false),
                    ListValue list => new BoolValue(Contains(list, a)),
                    _ => throw TypeError($"the right side of `in` must be a list, not {b.TypeName}"),
                };
            default:
                return Arithmetic(node.Op, a, b);
        }
    }

    private static bool Contains(ListValue list, ExpressionValue a)
    {
        for (int i = 0; i < list.Values.Count; i++)
        {
            if (ValuesEqual(a, list.Values[i]))
            {
                return true;
            }
        }
        return false;
    }

    private static BoolValue Ordering(ExpressionBinaryOp op, ExpressionValue a, ExpressionValue b)
    {
        if (a is NullValue || b is NullValue)
        {
            return new BoolValue(false);
        }
        int? ord = ExpressionFunctions.Compare(a, b);
        if (ord is null)
        {
            throw TypeError($"cannot order {a.TypeName} and {b.TypeName}");
        }
        bool result = op switch
        {
            ExpressionBinaryOp.Lt => ord.Value < 0,
            ExpressionBinaryOp.Le => ord.Value <= 0,
            ExpressionBinaryOp.Gt => ord.Value > 0,
            _ => ord.Value >= 0,
        };
        return new BoolValue(result);
    }

    private static long CheckedAdd(long x, long y)
    {
        try
        {
            return checked(x + y);
        }
        catch (OverflowException)
        {
            throw IntOverflow();
        }
    }

    private static long CheckedSub(long x, long y)
    {
        try
        {
            return checked(x - y);
        }
        catch (OverflowException)
        {
            throw IntOverflow();
        }
    }

    private static long CheckedMul(long x, long y)
    {
        try
        {
            return checked(x * y);
        }
        catch (OverflowException)
        {
            throw IntOverflow();
        }
    }

    private static long CheckedRem(long x, long y)
    {
        try
        {
            return checked(x % y);
        }
        catch (OverflowException)
        {
            throw IntOverflow();
        }
    }

    private static ExpressionValue Arithmetic(ExpressionBinaryOp op, ExpressionValue a, ExpressionValue b)
    {
        if (a is NullValue || b is NullValue)
        {
            return ExpressionValue.Null;
        }
        if (op == ExpressionBinaryOp.Add && a is TextValue x && b is TextValue y)
        {
            return new TextValue(x.Value + y.Value);
        }
        if (a is IntValue ix && b is IntValue iy)
        {
            switch (op)
            {
                case ExpressionBinaryOp.Add:
                    return new IntValue(CheckedAdd(ix.Value, iy.Value));
                case ExpressionBinaryOp.Sub:
                    return new IntValue(CheckedSub(ix.Value, iy.Value));
                case ExpressionBinaryOp.Mul:
                    return new IntValue(CheckedMul(ix.Value, iy.Value));
                case ExpressionBinaryOp.Rem:
                    if (iy.Value == 0)
                    {
                        throw DivisionByZero();
                    }
                    return new IntValue(CheckedRem(ix.Value, iy.Value));
                default:
                    break;
            }
        }
        ExpressionDecimal? xa = AsDecimal(a);
        ExpressionDecimal? xb = AsDecimal(b);
        if (xa is null || xb is null)
        {
            throw TypeError($"`{ExpressionBinaryOpLevels.Text(op)}` does not apply to {a.TypeName} and {b.TypeName}");
        }
        if (xb.Value.IsZero && (op == ExpressionBinaryOp.Div || op == ExpressionBinaryOp.Rem))
        {
            throw DivisionByZero();
        }
        ExpressionDecimal? result = op switch
        {
            ExpressionBinaryOp.Add => ExpressionDecimal.Add(xa.Value, xb.Value),
            ExpressionBinaryOp.Sub => ExpressionDecimal.Sub(xa.Value, xb.Value),
            ExpressionBinaryOp.Mul => ExpressionDecimal.Mul(xa.Value, xb.Value),
            ExpressionBinaryOp.Div => ExpressionDecimal.Div(xa.Value, xb.Value),
            _ => ExpressionDecimal.Rem(xa.Value, xb.Value),
        };
        return DecimalFrom(result);
    }
}