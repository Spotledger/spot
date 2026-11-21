using System.Text;

namespace Spotledger.Client.Assistant;

/// <summary>The kinds of inline run the sanitiser can produce.</summary>
public enum RunKind { Text, Bold, Code, Link }

/// <summary>An inline run; <see cref="Destination"/> is set only for registered links.</summary>
public sealed record MarkdownRun(RunKind Kind, string Text, string? Destination = null);

/// <summary>A block: a paragraph or a bullet item.</summary>
public sealed record MarkdownBlock(bool IsBullet, IReadOnlyList<MarkdownRun> Runs);

/// <summary>
/// Injection-safe rendering of assistant text. Only paragraphs, bullets, bold, inline code
/// and links survive. Markup characters such as angle brackets stay literal text (the view
/// never parses HTML), images are replaced by their alt text, and a link keeps its target
/// only when the destination is registered; otherwise it shows as plain text.
/// </summary>
public static class SafeMarkdown
{
    /// <summary>Parses <paramref name="source"/>; <paramref name="isRegistered"/> decides which link targets are allowed.</summary>
    public static IReadOnlyList<MarkdownBlock> Parse(string? source, Func<string, bool> isRegistered)
    {
        ArgumentNullException.ThrowIfNull(isRegistered);
        var blocks = new List<MarkdownBlock>();
        if (string.IsNullOrEmpty(source))
        {
            return blocks;
        }

        foreach (var raw in source.Replace("\r\n", "\n", StringComparison.Ordinal).Split('\n'))
        {
            var line = raw.Trim();
            if (line.Length == 0)
            {
                continue;
            }

            var bullet = line.StartsWith("- ", StringComparison.Ordinal) || line.StartsWith("* ", StringComparison.Ordinal);
            blocks.Add(new MarkdownBlock(bullet, Inline(bullet ? line[2..] : line, isRegistered)));
        }

        return blocks;
    }

    /// <summary>The plain text of the blocks, for accessibility names and copying.</summary>
    public static string ToPlainText(IEnumerable<MarkdownBlock> blocks) =>
        string.Join('\n', blocks.Select(b => (b.IsBullet ? "- " : string.Empty) + string.Concat(b.Runs.Select(r => r.Text))));

    private static List<MarkdownRun> Inline(string text, Func<string, bool> isRegistered)
    {
        var runs = new List<MarkdownRun>();
        var plain = new StringBuilder();
        void Flush()
        {
            if (plain.Length > 0)
            {
                runs.Add(new MarkdownRun(RunKind.Text, plain.ToString()));
                plain.Clear();
            }
        }

        var i = 0;
        while (i < text.Length)
        {
            if (text[i] == '!' && i + 1 < text.Length && text[i + 1] == '[' && TryLink(text, i + 1, out var imgLabel, out _, out var imgEnd))
            {
                plain.Append(imgLabel);
                i = imgEnd;
            }
            else if (text[i] == '[' && TryLink(text, i, out var label, out var dest, out var end))
            {
                Flush();
                runs.Add(isRegistered(dest)
                    ? new MarkdownRun(RunKind.Link, label, dest)
                    : new MarkdownRun(RunKind.Text, label));
                i = end;
            }
            else if (text.AsSpan(i).StartsWith("**", StringComparison.Ordinal) && text.IndexOf("**", i + 2, StringComparison.Ordinal) is var b and > 0)
            {
                Flush();
                runs.Add(new MarkdownRun(RunKind.Bold, text[(i + 2)..b]));
                i = b + 2;
            }
            else if (text[i] == '`' && text.IndexOf('`', i + 1) is var c and > 0)
            {
                Flush();
                runs.Add(new MarkdownRun(RunKind.Code, text[(i + 1)..c]));
                i = c + 1;
            }
            else
            {
                plain.Append(text[i]);
                i++;
            }
        }

        Flush();
        return runs;
    }

    private static bool TryLink(string text, int start, out string label, out string dest, out int end)
    {
        label = dest = string.Empty;
        end = start;
        var close = text.IndexOf(']', start + 1);
        if (close < 0 || close + 1 >= text.Length || text[close + 1] != '(')
        {
            return false;
        }

        var paren = text.IndexOf(')', close + 2);
        if (paren < 0)
        {
            return false;
        }

        label = text[(start + 1)..close];
        dest = text[(close + 2)..paren].Trim();
        end = paren + 1;
        return true;
    }
}
