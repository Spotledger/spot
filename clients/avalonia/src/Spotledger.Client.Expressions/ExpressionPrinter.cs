using System;
using System.Collections.Generic;
using System.Text;

namespace Spotledger.Client.Expressions;

/// <summary>
/// The canonical text of an expression (specification section 10): single spaces around binary
/// operators and after commas, the minimum parentheses the precedence table requires, text
/// literals double-quoted with the five escapes, decimals with their scale.
/// </summary>
public static class ExpressionPrinter
{
    /// <summary>Double-quoted text with <c>\\ \" \n \t \r</c> escapes (the Rust <c>quote</c>).</summary>
    internal static string Quote(string s)
    {
        StringBuilder out_ = new(s.Length + 2);
        out_.Append('"');
        foreach (char c in s)
        {
            switch (c)
            {
                case '\\': out_.Append("\\\\"); break;
                case '"': out_.Append("\\\""); break;
                case '\n': out_.Append("\\n"); break;
                case '\t': out_.Append("\\t"); break;
                case '\r': out_.Append("\\r"); break;
                default: out_.Append(c); break;
            }
        }
        out_.Append('"');
        return out_.ToString();
    }

    /// <summary>The precedence level of a node; primaries are 9.</summary>
    private static int Level(ExpressionNode e) => e switch
    {
        TernaryNode => 1,
        BinaryNode b => ExpressionBinaryOpLevels.Of(b.Op),
        UnaryNode u => u.Op == ExpressionUnaryOp.Not ? 4 : 8,
        _ => 9,
    };

    /// <summary>The canonical text of a node.</summary>
    public static string Print(ExpressionNode e)
    {
        StringBuilder out_ = new();
        Write(e, 1, out_);
        return out_.ToString();
    }

    private static void Write(ExpressionNode e, int minLevel, StringBuilder out_)
    {
        bool paren = Level(e) < minLevel;
        if (paren)
        {
            out_.Append('(');
        }
        switch (e)
        {
            case NullNode:
                out_.Append("null");
                break;
            case BoolNode b:
                out_.Append(b.Value ? "true" : "false");
                break;
            case IntNode n:
                out_.Append(ExpressionValue.IntToString(n.Value));
                break;
            case DecimalNode d:
                out_.Append(new ExpressionDecimal(d.Mantissa, d.Scale).ToString());
                break;
            case TextNode t:
                out_.Append(Quote(t.Value));
                break;
            case ListNode list:
                out_.Append('[');
                CommaSeparated(list.Items, out_);
                out_.Append(']');
                break;
            case PathNode path:
                out_.Append(ExpressionRootText.Of(path.Root));
                for (int i = 0; i < path.Names.Count; i++)
                {
                    out_.Append('.').Append(path.Names[i]);
                }
                break;
            case UnaryNode unary:
                if (unary.Op == ExpressionUnaryOp.Not)
                {
                    out_.Append("not ");
                    Write(unary.Inner, 4, out_);
                }
                else
                {
                    out_.Append('-');
                    Write(unary.Inner, 8, out_);
                }
                break;
            case BinaryNode binary:
                {
                    int own = ExpressionBinaryOpLevels.Of(binary.Op);
                    // Comparison is not associative: both sides must bind tighter. The other
                    // binary operators are left-associative: the right side must bind tighter.
                    (int leftMin, int rightMin) = own == 5 ? (6, 6) : (own, own + 1);
                    Write(binary.Left, leftMin, out_);
                    out_.Append(' ').Append(ExpressionBinaryOpLevels.Text(binary.Op)).Append(' ');
                    Write(binary.Right, rightMin, out_);
                    break;
                }
            case TernaryNode ternary:
                Write(ternary.Condition, 2, out_);
                out_.Append(" ? ");
                Write(ternary.WhenTrue, 1, out_);
                out_.Append(" : ");
                Write(ternary.WhenFalse, 1, out_);
                break;
            case CallNode call:
                out_.Append(call.Name).Append('(');
                CommaSeparated(call.Arguments, out_);
                out_.Append(')');
                break;
            default:
                throw new InvalidOperationException("unknown node");
        }
        if (paren)
        {
            out_.Append(')');
        }
    }

    private static void CommaSeparated(IReadOnlyList<ExpressionNode> items, StringBuilder out_)
    {
        for (int i = 0; i < items.Count; i++)
        {
            if (i > 0)
            {
                out_.Append(", ");
            }
            Write(items[i], 1, out_);
        }
    }
}