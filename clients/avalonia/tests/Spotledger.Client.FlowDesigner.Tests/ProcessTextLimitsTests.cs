using System;
using System.Linq;
using System.Text;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The reader's size limits: a document exactly at a limit reads, one over it is refused with the
/// right line and a message that names the limit but never echoes the offending text. Small limits
/// are passed in so the tests stay fast; the defaults are checked by value.
public sealed class ProcessTextLimitsTests
{
    private const string Header = "name = \"p\"\napp = \"a\"\nversion = 1\nstatus = \"draft\"\n";

    private static string Nodes(int count)
    {
        StringBuilder text = new(Header);
        for (int i = 0; i < count; i++)
        {
            text.Append("[[nodes]]\nid = \"n").Append(i).Append("\"\nkind = \"task\"\n");
        }

        return text.ToString();
    }

    private static FlowTextException Refused(string text, ProcessTextLimits limits) =>
        Assert.Throws<FlowTextException>(() => ProcessTextReader.Read(text, limits));

    [Fact]
    public void TheDefaultsAreTheDocumentedOnes()
    {
        ProcessTextLimits limits = ProcessTextLimits.Default;

        Assert.Equal(4 * 1024 * 1024, limits.MaxTextLength);
        Assert.Equal(50_000, limits.MaxLines);
        Assert.Equal(4_096, limits.MaxLineLength);
        Assert.Equal(5_000, limits.MaxNodes);
        Assert.Equal(20_000, limits.MaxEdges);
        Assert.Equal(5_000, limits.MaxRules);
        Assert.Equal(128, limits.MaxKeyLength);
        Assert.Equal(64 * 1024, limits.MaxValueLength);
    }

    [Fact]
    public void ReadWithoutLimitsStillReadsAnOrdinaryDocument()
    {
        Assert.Equal(2, ProcessTextReader.Read(Nodes(2)).Nodes.Count);
    }

    [Fact]
    public void TextExactlyAtTheSizeLimitReadsAndOneOverIsRefused()
    {
        string text = Nodes(1);
        Assert.Single(ProcessTextReader.Read(text, new ProcessTextLimits { MaxTextLength = text.Length }).Nodes);

        FlowTextException error = Refused(text, new ProcessTextLimits { MaxTextLength = text.Length - 1 });
        Assert.Contains("text size (limit " + (text.Length - 1) + ")", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void LinesExactlyAtTheLimitReadAndTheNextLineIsRefusedAtItsNumber()
    {
        string text = Nodes(1);
        int lines = text.Split('\n', StringSplitOptions.RemoveEmptyEntries).Length;
        Assert.Single(ProcessTextReader.Read(text, new ProcessTextLimits { MaxLines = lines }).Nodes);

        FlowTextException error = Refused(text, new ProcessTextLimits { MaxLines = lines - 1 });
        Assert.Equal(lines, error.Line);
        Assert.Contains("line count (limit " + (lines - 1) + ")", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void CarriageReturnsCountAsOneLineBreakEach()
    {
        string text = Nodes(1).Replace("\n", "\r\n", StringComparison.Ordinal);
        int lines = text.Split("\r\n", StringSplitOptions.RemoveEmptyEntries).Length;

        Assert.Single(ProcessTextReader.Read(text, new ProcessTextLimits { MaxLines = lines }).Nodes);
        Assert.Equal(lines, Refused(text, new ProcessTextLimits { MaxLines = lines - 1 }).Line);
    }

    [Fact]
    public void ALineExactlyAtTheLengthLimitReadsAndOneOverIsRefusedAtItsNumber()
    {
        string text = Header + "title = \"" + new string('x', 20) + "\"\n";
        int longest = text.Split('\n').Max(l => l.Length);
        Assert.NotNull(ProcessTextReader.Read(text, new ProcessTextLimits { MaxLineLength = longest }));

        FlowTextException error = Refused(text, new ProcessTextLimits { MaxLineLength = longest - 1 });
        Assert.Equal(5, error.Line);
        Assert.Contains("line length (limit " + (longest - 1) + ")", error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("xxxx", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void NodesExactlyAtTheLimitReadAndTheNextSectionIsRefusedAtItsHeader()
    {
        Assert.Equal(3, ProcessTextReader.Read(Nodes(3), new ProcessTextLimits { MaxNodes = 3 }).Nodes.Count);

        FlowTextException error = Refused(Nodes(4), new ProcessTextLimits { MaxNodes = 3 });
        Assert.Equal(5 + 3 * 3, error.Line);
        Assert.Contains("nodes", error.Message, StringComparison.Ordinal);
        Assert.Contains("(limit 3)", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void EdgesExactlyAtTheLimitReadAndTheNextIsRefused()
    {
        string head = Nodes(2);
        string two = head + "[[edges]]\nfrom = \"n0\"\nto = \"n1\"\n[[edges]]\nfrom = \"n1\"\nto = \"n0\"\n";
        Assert.Equal(2, ProcessTextReader.Read(two, new ProcessTextLimits { MaxEdges = 2 }).Edges.Count);

        string three = two + "[[edges]]\nfrom = \"n0\"\nto = \"n1\"\n";
        FlowTextException error = Refused(three, new ProcessTextLimits { MaxEdges = 2 });
        Assert.Equal(head.Split('\n').Length - 1 + 7, error.Line);
        Assert.Contains("(limit 2)", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void RulesExactlyAtTheLimitReadAndTheNextIsRefused()
    {
        string two = Header + "[[rules]]\nid = \"r1\"\nexpression = \"a\"\n[[rules]]\nid = \"r2\"\nexpression = \"b\"\n";
        Assert.Equal(2, ProcessTextReader.Read(two, new ProcessTextLimits { MaxRules = 2 }).Rules.Count);

        FlowTextException error = Refused(two + "[[rules]]\nid = \"r3\"\nexpression = \"c\"\n", new ProcessTextLimits { MaxRules = 2 });
        Assert.Equal(11, error.Line);
        Assert.Contains("rules", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AKeyExactlyAtTheLimitReadsAndOneOverIsRefused()
    {
        string text = Header + "[[nodes]]\nid = \"n\"\nkind = \"task\"\nabcdefgh = \"1\"\n";
        Assert.Single(ProcessTextReader.Read(text, new ProcessTextLimits { MaxKeyLength = 8 }).Nodes);

        FlowTextException error = Refused(text, new ProcessTextLimits { MaxKeyLength = 7 });
        Assert.Equal(8, error.Line);
        Assert.DoesNotContain("abcdefgh", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AValueExactlyAtTheLimitReadsAndOneOverIsRefused()
    {
        string text = Header + "title = \"abcdef\"\n";
        Assert.NotNull(ProcessTextReader.Read(text, new ProcessTextLimits { MaxValueLength = 8 }));

        FlowTextException error = Refused(text, new ProcessTextLimits { MaxValueLength = 7 });
        Assert.Equal(5, error.Line);
        Assert.DoesNotContain("abcdef", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void AHugeDocumentIsRefusedBeforeItIsSplit()
    {
        string text = new('\n', 10_000_000);

        FlowTextException error = Refused(text, ProcessTextLimits.Default);

        Assert.Equal(1, error.Line);
        Assert.Contains("text size", error.Message, StringComparison.Ordinal);
    }
}
