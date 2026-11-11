using System;
using System.Collections.Generic;

namespace Spotledger.Client.Expressions;

/// <summary>What an expression reads from the outside world: field values and <c>now</c>.
/// Nothing else is readable.</summary>
public interface IExpressionContext
{
    /// <summary>The value at <c>root.names...</c>, or nothing when there is none
    /// (which reads as <see cref="ExpressionValue.Null" />).</summary>
    ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names);

    /// <summary>The instant the function <c>now()</c> returns.</summary>
    ExpressionDateTime Now { get; }
}

/// <summary>
/// A context over a map from dotted path (<c>doc.qty</c>, <c>parent.status</c>) to value,
/// for tests, tools and hosts that keep flat records.
/// </summary>
public sealed class ExpressionMapContext : IExpressionContext
{
    private readonly Dictionary<string, ExpressionValue> values = new(StringComparer.Ordinal);

    /// <summary>An empty context at <paramref name="now" />.</summary>
    public ExpressionMapContext(ExpressionDateTime now) => Now = now;

    /// <summary>The instant the function <c>now()</c> returns.</summary>
    public ExpressionDateTime Now { get; }

    /// <summary>Sets the value of a dotted path and returns the context.</summary>
    public ExpressionMapContext With(string path, ExpressionValue value)
    {
        values[path] = value;
        return this;
    }

    /// <summary>The value at <c>root.names...</c>, or nothing.</summary>
    public ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names)
    {
        System.Text.StringBuilder key = new(ExpressionRootText.Of(root));
        for (int i = 0; i < names.Count; i++)
        {
            key.Append('.').Append(names[i]);
        }
        return values.TryGetValue(key.ToString(), out ExpressionValue? value) ? value : null;
    }
}