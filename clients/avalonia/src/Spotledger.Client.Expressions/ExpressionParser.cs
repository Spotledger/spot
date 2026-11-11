using System;
using System.Collections.Generic;
using System.Globalization;

namespace Spotledger.Client.Expressions;

/// <summary>
/// The parser: recursive descent over the grammar of specification section 3, with the node
/// and depth limits checked as each node is created (post-order), mirroring the Rust parser
/// exactly, including which of two simultaneous problems is reported.
/// </summary>
public static class ExpressionParser
{
    /// <summary>The most nodes an expression may have.</summary>
    public const int MaxNodes = 2000;

    /// <summary>The deepest nesting an expression may have; the root is depth 1.</summary>
    public const int MaxDepth = 64;

    /// <summary>Parses an expression, or throws <see cref="ExpressionParseException" />.</summary>
    public static ExpressionNode Parse(string source)
    {
        List<Token> tokens = ExpressionLexer.Lex(source);
        Parser p = new(tokens);
        (ExpressionNode expr, int _) = p.Ternary();
        Token after = p.Peek();
        if (after.Kind != TokKind.End)
        {
            throw p.Unexpected();
        }
        return expr;
    }

    private sealed class Parser
    {
        private readonly List<Token> tokens;
        private int at;
        private int nodes;

        internal Parser(List<Token> tokens) => this.tokens = tokens;

        internal Token Peek() => tokens[Math.Min(at, tokens.Count - 1)];

        internal Token Bump()
        {
            Token t = Peek();
            if (t.Kind != TokKind.End)
            {
                at += 1;
            }
            return t;
        }

        internal ExpressionParseException Unexpected()
        {
            Token t = Peek();
            string what = t.Kind == TokKind.End ? "the end of the expression" : $"`{ExpressionLexer.Describe(t)}`";
            return new ExpressionParseException(ExpressionErrorCode.Syntax, "unexpected " + what, t.Offset);
        }

        internal void Expect(TokKind want)
        {
            if (Peek().Kind != want)
            {
                throw Unexpected();
            }
            Bump();
        }

        /// <summary>Counts a node and checks both limits at creation (post-order).</summary>
        internal (ExpressionNode, int) Node(ExpressionNode expr, int childrenDepth, int offset)
        {
            nodes += 1;
            if (nodes > MaxNodes)
            {
                throw new ExpressionParseException(
                    ExpressionErrorCode.TooComplex,
                    $"an expression has at most {MaxNodes} nodes",
                    offset);
            }
            int depth = childrenDepth + 1;
            if (depth > MaxDepth)
            {
                throw new ExpressionParseException(
                    ExpressionErrorCode.TooDeep,
                    $"an expression nests at most {MaxDepth} deep",
                    offset);
            }
            expr.Offset = offset;
            return (expr, depth);
        }

        internal (ExpressionNode, int) Ternary()
        {
            int offset = Peek().Offset;
            (ExpressionNode cond, int condDepth) = Or();
            if (Peek().Kind != TokKind.Question)
            {
                return (cond, condDepth);
            }
            Bump();
            (ExpressionNode yes, int yesDepth) = Ternary();
            Expect(TokKind.Colon);
            (ExpressionNode no, int noDepth) = Ternary();
            int depth = Math.Max(condDepth, Math.Max(yesDepth, noDepth));
            return Node(
                new TernaryNode(cond, yes, no),
                depth,
                offset);
        }

        private (ExpressionNode, int) BinaryChain(
            Func<(ExpressionNode, int)> next,
            (TokKind, ExpressionBinaryOp)[] ops)
        {
            int offset = Peek().Offset;
            (ExpressionNode left, int leftDepth) = next();
            while (true)
            {
                ExpressionBinaryOp? op = null;
                foreach ((TokKind kind, ExpressionBinaryOp candidate) in ops)
                {
                    if (Peek().Kind == kind)
                    {
                        op = candidate;
                        break;
                    }
                }
                if (op is null)
                {
                    return (left, leftDepth);
                }
                Bump();
                (ExpressionNode right, int rightDepth) = next();
                int depth = Math.Max(leftDepth, rightDepth);
                (left, leftDepth) = Node(
                    new BinaryNode(op.Value, left, right),
                    depth,
                    offset);
            }
        }

        internal (ExpressionNode, int) Or() =>
            BinaryChain(And, new[] { (TokKind.Or, ExpressionBinaryOp.Or) });

        internal (ExpressionNode, int) And() =>
            BinaryChain(NotChain, new[] { (TokKind.And, ExpressionBinaryOp.And) });

        internal (ExpressionNode, int) NotChain()
        {
            if (Peek().Kind == TokKind.Not)
            {
                int offset = Bump().Offset;
                (ExpressionNode inner, int innerDepth) = NotChain();
                return Node(
                    new UnaryNode(ExpressionUnaryOp.Not, inner),
                    innerDepth,
                    offset);
            }
            return Comparison();
        }

        internal (ExpressionNode, int) Comparison()
        {
            int offset = Peek().Offset;
            (ExpressionNode left, int leftDepth) = Additive();
            ExpressionBinaryOp? op = Peek().Kind switch
            {
                TokKind.EqEq => ExpressionBinaryOp.Eq,
                TokKind.NotEq => ExpressionBinaryOp.Ne,
                TokKind.Lt => ExpressionBinaryOp.Lt,
                TokKind.Le => ExpressionBinaryOp.Le,
                TokKind.Gt => ExpressionBinaryOp.Gt,
                TokKind.Ge => ExpressionBinaryOp.Ge,
                TokKind.In => ExpressionBinaryOp.In,
                _ => null,
            };
            if (op is null)
            {
                return (left, leftDepth);
            }
            Bump();
            (ExpressionNode right, int rightDepth) = Additive();
            int depth = Math.Max(leftDepth, rightDepth);
            return Node(
                new BinaryNode(op.Value, left, right),
                depth,
                offset);
        }

        internal (ExpressionNode, int) Additive() =>
            BinaryChain(
                Multiplicative,
                new[] { (TokKind.Plus, ExpressionBinaryOp.Add), (TokKind.Minus, ExpressionBinaryOp.Sub) });

        internal (ExpressionNode, int) Multiplicative() =>
            BinaryChain(
                Unary,
                new[]
                {
                    (TokKind.Star, ExpressionBinaryOp.Mul),
                    (TokKind.Slash, ExpressionBinaryOp.Div),
                    (TokKind.Percent, ExpressionBinaryOp.Rem),
                });

        internal (ExpressionNode, int) Unary()
        {
            if (Peek().Kind == TokKind.Minus)
            {
                int offset = Bump().Offset;
                (ExpressionNode inner, int innerDepth) = Unary();
                return Node(
                    new UnaryNode(ExpressionUnaryOp.Neg, inner),
                    innerDepth,
                    offset);
            }
            return Primary();
        }

        internal (ExpressionNode, int) Primary()
        {
            Token token = Peek();
            int offset = token.Offset;
            switch (token.Kind)
            {
                case TokKind.Int:
                    Bump();
                    return Node(new IntNode(token.IntValue), 0, offset);
                case TokKind.Dec:
                    Bump();
                    return Node(new DecimalNode(token.DecValue.Mantissa, token.DecValue.Scale), 0, offset);
                case TokKind.Text:
                    Bump();
                    return Node(new TextNode(token.TextValue), 0, offset);
                case TokKind.True:
                    Bump();
                    return Node(new BoolNode(true), 0, offset);
                case TokKind.False:
                    Bump();
                    return Node(new BoolNode(false), 0, offset);
                case TokKind.Null:
                    Bump();
                    return Node(new NullNode(), 0, offset);
                case TokKind.LParen:
                    {
                        Bump();
                        (ExpressionNode inner, int innerDepth) = Ternary();
                        Expect(TokKind.RParen);
                        return (inner, innerDepth);
                    }
                case TokKind.LBracket:
                    {
                        Bump();
                        (List<ExpressionNode> items, int depth) = Arguments(TokKind.RBracket);
                        return Node(new ListNode(items), depth, offset);
                    }
                case TokKind.Ident:
                    {
                        string name = token.TextValue;
                        Bump();
                        if (Peek().Kind == TokKind.LParen)
                        {
                            Bump();
                            (List<ExpressionNode> args, int depth) = Arguments(TokKind.RParen);
                            (int min, bool atLeast)? arity = ExpressionFunctions.Arity(name);
                            if (arity is null)
                            {
                                throw new ExpressionParseException(
                                    ExpressionErrorCode.UnknownFunction,
                                    $"`{name}` is not a function of the language",
                                    offset);
                            }
                            bool accepts = arity.Value.atLeast ? args.Count >= arity.Value.min : args.Count == arity.Value.min;
                            if (!accepts)
                            {
                                throw new ExpressionParseException(
                                    ExpressionErrorCode.Arity,
                                    $"`{name}` does not take {args.Count} arguments",
                                    offset);
                            }
                            return Node(new CallNode(name, args), depth, offset);
                        }
                        ExpressionRoot? root = name switch
                        {
                            "doc" => ExpressionRoot.Doc,
                            "parent" => ExpressionRoot.Parent,
                            _ => null,
                        };
                        if (root is null)
                        {
                            throw new ExpressionParseException(
                                ExpressionErrorCode.UnknownName,
                                $"`{name}` is not a name in the language",
                                offset);
                        }
                        List<string> path = new();
                        while (Peek().Kind == TokKind.Dot)
                        {
                            Bump();
                            Token segment = Bump();
                            if (segment.Kind != TokKind.Ident)
                            {
                                at -= 1;
                                throw Unexpected();
                            }
                            path.Add(segment.TextValue);
                        }
                        if (path.Count == 0)
                        {
                            throw new ExpressionParseException(
                                ExpressionErrorCode.UnknownName,
                                $"`{name}` must be followed by a field name",
                                offset);
                        }
                        return Node(new PathNode(root.Value, path), 0, offset);
                    }
                default:
                    throw Unexpected();
            }
        }

        /// <summary>Comma-separated expressions up to the closing token; the deepest child's depth.</summary>
        internal (List<ExpressionNode>, int) Arguments(TokKind close)
        {
            List<ExpressionNode> items = new();
            int depth = 0;
            if (Peek().Kind == close)
            {
                Bump();
                return (items, depth);
            }
            while (true)
            {
                (ExpressionNode e, int d) = Ternary();
                items.Add(e);
                depth = Math.Max(depth, d);
                if (Peek().Kind == TokKind.Comma)
                {
                    Bump();
                    continue;
                }
                if (Peek().Kind == close)
                {
                    Bump();
                    return (items, depth);
                }
                throw Unexpected();
            }
        }
    }
}