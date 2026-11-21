using System.Globalization;
using System.Text;

namespace Spotledger.Client.Extensions;

/// <summary>One inline piece of a text block.</summary>
public abstract record MarkdownInline;

/// <summary>Literal text.</summary>
/// <param name="Text">The text.</param>
public sealed record MarkdownText(string Text) : MarkdownInline;

/// <summary>Emphasised text.</summary>
/// <param name="Children">The emphasised inlines.</param>
public sealed record MarkdownEmphasis(IReadOnlyList<MarkdownInline> Children) : MarkdownInline;

/// <summary>Strong text.</summary>
/// <param name="Children">The strong inlines.</param>
public sealed record MarkdownStrong(IReadOnlyList<MarkdownInline> Children) : MarkdownInline;

/// <summary>Inline code.</summary>
/// <param name="Text">The code text.</param>
public sealed record MarkdownCode(string Text) : MarkdownInline;

/// <summary>A link to a registered destination (the only link the subset has).</summary>
/// <param name="Text">The link's text.</param>
/// <param name="Destination">The destination id the <c>dest:</c> prefix named.</param>
public sealed record MarkdownLink(string Text, string Destination) : MarkdownInline;

/// <summary>One block of a text block's markdown.</summary>
public abstract record MarkdownBlock;

/// <summary>A paragraph.</summary>
/// <param name="Inlines">The paragraph's inlines.</param>
public sealed record MarkdownParagraph(IReadOnlyList<MarkdownInline> Inlines) : MarkdownBlock;

/// <summary>A list; one entry per item.</summary>
/// <param name="Ordered">True for a numbered list.</param>
/// <param name="Items">The items' inlines.</param>
public sealed record MarkdownList(bool Ordered, IReadOnlyList<IReadOnlyList<MarkdownInline>> Items) : MarkdownBlock;

/// <summary>
/// A <c>text</c> block's restricted markdown: paragraphs, emphasis, strong, inline code and
/// lists, with links only to registered destinations. Nothing here evaluates, fetches or
/// renders markup: the renderer draws exactly these nodes, so a document cannot smuggle
/// HTML, an image or an external link into the client.
/// </summary>
public sealed class MarkdownModel
{
    /// <summary>An empty model: nothing to draw.</summary>
    public static MarkdownModel Empty { get; } = new([], []);

    private MarkdownModel(IReadOnlyList<MarkdownBlock> blocks, IReadOnlyList<string> violations)
    {
        Blocks = blocks;
        Violations = violations;
    }

    /// <summary>The parsed blocks, in order.</summary>
    public IReadOnlyList<MarkdownBlock> Blocks { get; }

    /// <summary>Why the subset was refused; empty when it was not.</summary>
    public IReadOnlyList<string> Violations { get; }

    /// <summary>True when the text broke the subset (markup, an image, an external link).</summary>
    public bool IsRefused => Violations.Count > 0;

    /// <summary>Every destination the text links to, in order.</summary>
    public IReadOnlyList<string> Destinations
    {
        get
        {
            List<string> found = [];
            foreach (MarkdownBlock block in Blocks)
            {
                switch (block)
                {
                    case MarkdownParagraph paragraph:
                        Collect(paragraph.Inlines, found);
                        break;
                    case MarkdownList list:
                        foreach (IReadOnlyList<MarkdownInline> item in list.Items)
                        {
                            Collect(item, found);
                        }
                        break;
                    default:
                        break;
                }
            }
            return found;
        }
    }

    /// <summary>
    /// Parses a text block's markdown. Never throws: a violation is recorded and the model
    /// is refused, so the caller drops the block and reports <c>uiext.raw_html</c>.
    /// </summary>
    public static MarkdownModel Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        List<string> violations = [];
        if (text.Length > ExtensionLimits.MaxMarkdown)
        {
            violations.Add("the text is longer than the subset allows");
            return new MarkdownModel([], violations);
        }
        ScanRefusals(text, violations);
        if (violations.Count > 0)
        {
            return new MarkdownModel([], violations);
        }
        List<MarkdownBlock> blocks = [];
        foreach (string chunk in SplitBlocks(text))
        {
            string trimmed = chunk.Trim();
            if (trimmed.Length == 0)
            {
                continue;
            }
            if (IsListLine(trimmed, out _))
            {
                ParseList(chunk, blocks);
            }
            else
            {
                blocks.Add(new MarkdownParagraph(ParseInlines(trimmed)));
            }
        }
        return new MarkdownModel(blocks, violations);
    }

    private static void ScanRefusals(string text, List<string> violations)
    {
        for (int index = 0; index < text.Length; index++)
        {
            char character = text[index];
            if (character == '<' && index + 1 < text.Length && IsMarkupStart(text[index + 1]))
            {
                violations.Add("markup is not allowed");
                return;
            }
            if (character == '!' && index + 1 < text.Length && text[index + 1] == '[')
            {
                violations.Add("an image is not allowed");
                return;
            }
            if (character == ']' && index + 1 < text.Length && text[index + 1] == '(')
            {
                int close = text.IndexOf(')', index + 2);
                string target = close < 0 ? text[(index + 2)..] : text[(index + 2)..close];
                if (!target.StartsWith("dest:", StringComparison.Ordinal))
                {
                    violations.Add("a link must name a dest: destination");
                    return;
                }
            }
        }
    }

    private static bool IsMarkupStart(char next) =>
        char.IsAsciiLetter(next) || next is '/' or '!' or '?';

    private static IEnumerable<string> SplitBlocks(string text)
    {
        StringBuilder current = new();
        string normalized = text.Replace("\r\n", "\n", StringComparison.Ordinal).Replace('\r', '\n');
        foreach (string line in normalized.Split('\n'))
        {
            if (line.Trim().Length == 0)
            {
                if (current.Length > 0)
                {
                    yield return current.ToString();
                    current.Clear();
                }
                continue;
            }
            if (current.Length > 0)
            {
                current.Append('\n');
            }
            current.Append(line);
        }
        if (current.Length > 0)
        {
            yield return current.ToString();
        }
    }

    private static bool IsListLine(string line, out bool ordered)
    {
        ordered = false;
        if (line.StartsWith("- ", StringComparison.Ordinal))
        {
            return true;
        }
        int digits = 0;
        while (digits < line.Length && char.IsAsciiDigit(line[digits]))
        {
            digits++;
        }
        if (digits > 0 && digits + 1 < line.Length && line[digits] == '.' && line[digits + 1] == ' ')
        {
            ordered = true;
            return true;
        }
        return false;
    }

    private static void ParseList(string chunk, List<MarkdownBlock> blocks)
    {
        bool ordered = false;
        bool first = true;
        List<IReadOnlyList<MarkdownInline>> items = [];
        foreach (string line in chunk.Split('\n'))
        {
            string trimmed = line.Trim();
            if (!IsListLine(trimmed, out bool lineOrdered))
            {
                items.Add(ParseInlines(trimmed));
                continue;
            }
            if (first)
            {
                ordered = lineOrdered;
                first = false;
            }
            string body = trimmed.StartsWith("- ", StringComparison.Ordinal)
                ? trimmed[2..]
                : trimmed[(trimmed.IndexOf(' ', StringComparison.Ordinal) + 1)..];
            items.Add(ParseInlines(body.Trim()));
        }
        blocks.Add(new MarkdownList(ordered, items));
    }

    private static List<MarkdownInline> ParseInlines(string text)
    {
        List<MarkdownInline> inlines = [];
        StringBuilder literal = new();
        int index = 0;
        while (index < text.Length)
        {
            char character = text[index];
            if (character == '\\' && index + 1 < text.Length)
            {
                literal.Append(text[index + 1]);
                index += 2;
                continue;
            }
            if (character == '*' && index + 1 < text.Length && text[index + 1] == '*' &&
                TryFind(text, "**", index + 2, out int strongEnd))
            {
                Flush(literal, inlines);
                inlines.Add(new MarkdownStrong(ParseInlines(text[(index + 2)..strongEnd])));
                index = strongEnd + 2;
                continue;
            }
            if ((character == '*' || character == '_') && TryFind(text, character.ToString(), index + 1, out int emphasisEnd) && emphasisEnd > index + 1)
            {
                Flush(literal, inlines);
                inlines.Add(new MarkdownEmphasis(ParseInlines(text[(index + 1)..emphasisEnd])));
                index = emphasisEnd + 1;
                continue;
            }
            if (character == '`' && TryFind(text, "`", index + 1, out int codeEnd))
            {
                Flush(literal, inlines);
                inlines.Add(new MarkdownCode(text[(index + 1)..codeEnd]));
                index = codeEnd + 1;
                continue;
            }
            if (character == '[' && TryFind(text, "](", index + 1, out int labelEnd))
            {
                int close = text.IndexOf(')', labelEnd + 2);
                if (close > 0)
                {
                    string label = text[(index + 1)..labelEnd];
                    string target = text[(labelEnd + 2)..close];
                    if (target.StartsWith("dest:", StringComparison.Ordinal) && target.Length > 5)
                    {
                        Flush(literal, inlines);
                        inlines.Add(new MarkdownLink(label, target[5..]));
                        index = close + 1;
                        continue;
                    }
                }
            }
            literal.Append(character);
            index++;
        }
        Flush(literal, inlines);
        return inlines;
    }

    private static void Flush(StringBuilder literal, List<MarkdownInline> inlines)
    {
        if (literal.Length > 0)
        {
            inlines.Add(new MarkdownText(literal.ToString()));
            literal.Clear();
        }
    }

    private static bool TryFind(string text, string needle, int from, out int found)
    {
        found = text.IndexOf(needle, from, StringComparison.Ordinal);
        return found >= 0;
    }

    private static void Collect(IReadOnlyList<MarkdownInline> inlines, List<string> destinations)
    {
        foreach (MarkdownInline inline in inlines)
        {
            switch (inline)
            {
                case MarkdownLink link:
                    destinations.Add(link.Destination);
                    break;
                case MarkdownEmphasis emphasis:
                    Collect(emphasis.Children, destinations);
                    break;
                case MarkdownStrong strong:
                    Collect(strong.Children, destinations);
                    break;
                default:
                    break;
            }
        }
    }

    /// <summary>The plain text of the model (accessible names and tests).</summary>
    public string ToPlainText()
    {
        StringBuilder text = new();
        foreach (MarkdownBlock block in Blocks)
        {
            if (text.Length > 0)
            {
                text.Append('\n');
            }
            switch (block)
            {
                case MarkdownParagraph paragraph:
                    Append(paragraph.Inlines, text);
                    break;
                case MarkdownList list:
                    for (int index = 0; index < list.Items.Count; index++)
                    {
                        if (index > 0)
                        {
                            text.Append('\n');
                        }
                        Append(list.Items[index], text);
                    }
                    break;
                default:
                    break;
            }
        }
        return text.ToString();
    }

    /// <inheritdoc/>
    public override string ToString() => ToPlainText();

    private static void Append(IReadOnlyList<MarkdownInline> inlines, StringBuilder text)
    {
        foreach (MarkdownInline inline in inlines)
        {
            switch (inline)
            {
                case MarkdownText literal:
                    text.Append(literal.Text);
                    break;
                case MarkdownCode code:
                    text.Append(code.Text);
                    break;
                case MarkdownLink link:
                    text.Append(link.Text).Append(" (").Append(link.Destination).Append(')');
                    break;
                case MarkdownEmphasis emphasis:
                    Append(emphasis.Children, text);
                    break;
                case MarkdownStrong strong:
                    Append(strong.Children, text);
                    break;
                default:
                    break;
            }
        }
    }

    /// <summary>The index of a list item, for a renderer that numbers an ordered list.</summary>
    public static string ItemMarker(bool ordered, int index) =>
        ordered ? (index + 1).ToString(CultureInfo.InvariantCulture) + "." : "\u2022";
}
