using System.Collections.Generic;

namespace Spotledger.Client.Expressions;

/// <summary>The root of a field path.</summary>
public enum ExpressionRoot
{
    /// <summary>The document being edited.</summary>
    Doc,
    /// <summary>Its parent document (for a child row).</summary>
    Parent,
}

internal static class ExpressionRootText
{
    internal static string Of(ExpressionRoot root) => root == ExpressionRoot.Doc ? "doc" : "parent";
}

/// <summary>A unary operator.</summary>
public enum ExpressionUnaryOp
{
    /// <summary><c>not</c>.</summary>
    Not,
    /// <summary><c>-</c>.</summary>
    Neg,
}

/// <summary>A binary operator.</summary>
public enum ExpressionBinaryOp
{
    /// <summary><c>or</c>.</summary>
    Or,
    /// <summary><c>and</c>.</summary>
    And,
    /// <summary><c>==</c>.</summary>
    Eq,
    /// <summary><c>!=</c>.</summary>
    Ne,
    /// <summary><c>&lt;</c>.</summary>
    Lt,
    /// <summary><c>&lt;=</c>.</summary>
    Le,
    /// <summary><c>&gt;</c>.</summary>
    Gt,
    /// <summary><c>&gt;=</c>.</summary>
    Ge,
    /// <summary><c>in</c>.</summary>
    In,
    /// <summary><c>+</c>.</summary>
    Add,
    /// <summary><c>-</c>.</summary>
    Sub,
    /// <summary><c>*</c>.</summary>
    Mul,
    /// <summary><c>/</c>.</summary>
    Div,
    /// <summary><c>%</c>.</summary>
    Rem,
}

/// <summary>The precedence level of a binary operator (specification section 3).</summary>
internal static class ExpressionBinaryOpLevels
{
    internal static int Of(ExpressionBinaryOp op) => op switch
    {
        ExpressionBinaryOp.Or => 2,
        ExpressionBinaryOp.And => 3,
        ExpressionBinaryOp.Eq or ExpressionBinaryOp.Ne or ExpressionBinaryOp.Lt
            or ExpressionBinaryOp.Le or ExpressionBinaryOp.Gt or ExpressionBinaryOp.Ge
            or ExpressionBinaryOp.In => 5,
        ExpressionBinaryOp.Add or ExpressionBinaryOp.Sub => 6,
        _ => 7,
    };

    /// <summary>The operator's spelling.</summary>
    internal static string Text(ExpressionBinaryOp op) => op switch
    {
        ExpressionBinaryOp.Or => "or",
        ExpressionBinaryOp.And => "and",
        ExpressionBinaryOp.Eq => "==",
        ExpressionBinaryOp.Ne => "!=",
        ExpressionBinaryOp.Lt => "<",
        ExpressionBinaryOp.Le => "<=",
        ExpressionBinaryOp.Gt => ">",
        ExpressionBinaryOp.Ge => ">=",
        ExpressionBinaryOp.In => "in",
        ExpressionBinaryOp.Add => "+",
        ExpressionBinaryOp.Sub => "-",
        ExpressionBinaryOp.Mul => "*",
        ExpressionBinaryOp.Div => "/",
        _ => "%",
    };
}

/// <summary>
/// An expression node. Positions are character offsets into the source (0-based, counting
/// Unicode scalar values), needed by tests and tooling; structural tree equality goes through
/// <see cref="TreeEquals" />, which ignores them.
/// </summary>
public abstract record ExpressionNode
{
    /// <summary>The character offset of the node's first token in the source.</summary>
    public int Offset { get; internal set; }

    /// <summary>Structural equality over the tree, ignoring offsets: equal trees print identically.</summary>
    public bool TreeEquals(ExpressionNode? other) =>
        other is not null && ToString() == other.ToString();

    /// <summary>The tree's canonical text (see <see cref="ExpressionPrinter" />).</summary>
    public sealed override string ToString() => ExpressionPrinter.Print(this);
}

/// <summary>The literal <c>null</c>.</summary>
public sealed record NullNode : ExpressionNode;

/// <summary>The literal <c>true</c> or <c>false</c>.</summary>
public sealed record BoolNode(bool Value) : ExpressionNode;

/// <summary>An integer literal (never negative: <c>-5</c> is Neg(Int(5))).</summary>
public sealed record IntNode(long Value) : ExpressionNode;

/// <summary>A decimal literal with its written scale (mantissa digits as an Int128).</summary>
public sealed record DecimalNode(Int128 Mantissa, int Scale) : ExpressionNode;

/// <summary>A text literal.</summary>
public sealed record TextNode(string Value) : ExpressionNode;

/// <summary>A list of expressions.</summary>
public sealed record ListNode(IReadOnlyList<ExpressionNode> Items) : ExpressionNode;

/// <summary>A field path: the root, then the names.</summary>
public sealed record PathNode(ExpressionRoot Root, IReadOnlyList<string> Names) : ExpressionNode;

/// <summary>A unary operation (<c>not</c> or prefix <c>-</c>).</summary>
public sealed record UnaryNode(ExpressionUnaryOp Op, ExpressionNode Inner) : ExpressionNode;

/// <summary>A binary operation.</summary>
public sealed record BinaryNode(ExpressionBinaryOp Op, ExpressionNode Left, ExpressionNode Right) : ExpressionNode;

/// <summary>The conditional <c>c ? a : b</c>.</summary>
public sealed record TernaryNode(ExpressionNode Condition, ExpressionNode WhenTrue, ExpressionNode WhenFalse) : ExpressionNode;

/// <summary>A call of a library function.</summary>
public sealed record CallNode(string Name, IReadOnlyList<ExpressionNode> Arguments) : ExpressionNode;