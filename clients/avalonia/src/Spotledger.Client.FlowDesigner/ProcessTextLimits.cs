using System.Globalization;

namespace Spotledger.Client.FlowDesigner;

/// The size limits <see cref="ProcessTextReader"/> enforces. A process text comes from the server
/// and from files a user opens, so none of it is trusted to be small: every limit is checked
/// before the memory it protects is allocated, and a refusal names the limit and its value but
/// never echoes the offending text.
public sealed record ProcessTextLimits
{
    /// The defaults <see cref="ProcessTextReader.Read(string)"/> uses.
    public static ProcessTextLimits Default { get; } = new();

    /// Longest accepted text, in characters (4 MiB).
    public int MaxTextLength { get; init; } = 4 * 1024 * 1024;

    /// Most lines in a document.
    public int MaxLines { get; init; } = 50_000;

    /// Longest single line, in characters.
    public int MaxLineLength { get; init; } = 4_096;

    /// Most <c>[[nodes]]</c> sections.
    public int MaxNodes { get; init; } = 5_000;

    /// Most <c>[[edges]]</c> sections.
    public int MaxEdges { get; init; } = 20_000;

    /// Most <c>[[rules]]</c> sections.
    public int MaxRules { get; init; } = 5_000;

    /// Longest key, in characters.
    public int MaxKeyLength { get; init; } = 128;

    /// Longest value (the literal after the equals sign), in characters (64 KiB).
    public int MaxValueLength { get; init; } = 64 * 1024;

    internal static string Describe(string what, int limit) =>
        what + " (limit " + limit.ToString(CultureInfo.InvariantCulture) + ")";
}
