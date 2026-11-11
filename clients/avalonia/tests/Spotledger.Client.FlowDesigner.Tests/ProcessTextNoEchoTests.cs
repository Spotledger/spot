using System;
using System.IO;
using System.Linq;
using System.Text.RegularExpressions;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// Recorded security finding: a process document can come from a file a user was sent, and an error message is shown
/// in the UI and written to logs. No message may echo an input token: not a secret pasted into the
/// wrong field, not a control character that spoofs the screen, not a 2,000 character value. The
/// message still names the line and the kind of fault. A source guard keeps new error sites honest.
public sealed class ProcessTextNoEchoTests
{
    private const string Header = "name = \"p\"\napp = \"a\"\nversion = 1\nstatus = \"draft\"\n";
    private const string Secret = "sk_live_TOPSECRET";
    private static readonly string Long = new('Q', 2000);

    // Quoted-string payload: a secret, an ESC and a BEL control character, then 2,000 characters.
    private static readonly string Token = Secret + "\u001b[31m\u0007" + Long;

    // A key-shaped payload (keys are capped at 128 characters).
    private static readonly string KeyToken = Secret + "\u001b\u0007" + new string('Q', 80);

    public static TheoryData<string, string, int> Cases => new()
    {
        { Header + "[[nodes]]\nid = \"" + Token + "\"\nkind = \"task\"\n", "`id` is not a valid id", 6 },
        { Header + "[[nodes]]\nid = \"n1\"\nkind = \"task\"\nerror = \"" + Token + "\"\n", "`error` is not a valid id", 8 },
        { Header + "[[edges]]\nfrom = \"" + Token + "\"\nto = \"b\"\n", "`from` is not a valid id", 6 },
        { Header + "[[edges]]\nfrom = \"a\"\nto = \"" + Token + "\"\n", "`to` is not a valid id", 7 },
        { Header + "[[nodes]]\nid = \"n1\"\nkind = \"task\"\njoin = \"" + Token + "\"\n", "unknown join rule", 8 },
        { Header + "[[edges]]\nfrom = \"a\"\nto = \"b\"\n" + KeyToken + " = \"x\"\n", "unknown key", 8 },
        { Header + "[[rules]]\nid = \"r1\"\nexpression = \"c\"\n" + KeyToken + " = \"x\"\n", "unknown key", 8 },
        { "name = \"p\"\napp = \"a\"\nversion = 1\nstatus = \"" + Token + "\"\n", "unknown status", 4 },
        { Header + "[[nodes]]\nid = \"n1\"\nkind = \"task\"\n" + KeyToken + " = 1\n" + KeyToken + " = 2\n", "a key appears twice", 9 },
        { Header + "[[nodes]]\nid = \"n1\"\nkind = \"task\"\nerror = \"\\Z" + Token + "\"\n", "unknown escape", 8 },
        { Header + "[[nodes]]\nid = " + Token + "\nkind = \"task\"\n", "`id` must be a string", 6 },
    };

    [Theory]
    [MemberData(nameof(Cases))]
    public void AnErrorNamesTheLineAndTheFaultButNeverTheInput(string text, string kind, int line)
    {
        FlowTextException error = Assert.Throws<FlowTextException>(() => ProcessTextReader.Read(text));

        Assert.Equal(line, error.Line);
        Assert.StartsWith("line " + line + ": ", error.Message, StringComparison.Ordinal);
        Assert.Contains(kind, error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("TOPSECRET", error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("QQQQQ", error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("\\Z", error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain(error.Message, char.IsControl);
        Assert.DoesNotContain("TOPSECRET", error.Problem, StringComparison.Ordinal);
        Assert.True(error.Message.Length < 300, "the message is short: " + error.Message.Length);
    }

    [Fact]
    public void AnUnknownKeyListsTheKeysTheFormatDefinesAndNotTheOffendingOne()
    {
        string text = Header + "[[edges]]\nfrom = \"a\"\nto = \"b\"\ncolour = \"red\"\n";

        FlowTextException error = Assert.Throws<FlowTextException>(() => ProcessTextReader.Read(text));

        Assert.Contains("`from`", error.Message, StringComparison.Ordinal);
        Assert.Contains("`condition`", error.Message, StringComparison.Ordinal);
        Assert.DoesNotContain("colour", error.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void TheAtHelpersBuildTheMessageFromTheKindAndTheLineOnly()
    {
        FlowTextException plain = FlowTextException.At(7, "a thing is wrong");
        FlowTextException known = FlowTextException.AtKey(7, "id", "is not a valid id");
        FlowTextException foreign = FlowTextException.AtKey(7, Secret, "appears twice");

        Assert.Equal("line 7: a thing is wrong", plain.Message);
        Assert.Equal("line 7: `id` is not a valid id", known.Message);
        Assert.Equal("line 7: a key appears twice", foreign.Message);
        Assert.Equal(7, foreign.Line);
    }

    [Fact]
    public void NoErrorSiteInTheSourceBuildsItsMessageByConcatenation()
    {
        string source = SourceDirectory();
        int checkedSites = 0;
        foreach (string file in Directory.GetFiles(source, "*.cs", SearchOption.AllDirectories))
        {
            if (file.Contains(Path.DirectorySeparatorChar + "obj" + Path.DirectorySeparatorChar, StringComparison.Ordinal)
                || file.Contains(Path.DirectorySeparatorChar + "bin" + Path.DirectorySeparatorChar, StringComparison.Ordinal)
                || string.Equals(Path.GetFileName(file), "FlowTextException.cs", StringComparison.Ordinal))
            {
                continue;
            }

            string text = File.ReadAllText(file);
            Assert.DoesNotContain("new FlowTextException(", text, StringComparison.Ordinal);
            foreach (Match site in Regex.Matches(text, @"FlowTextException\s*\.\s*At\w*\s*\((?<args>[^;]*);"))
            {
                checkedSites++;
                string args = site.Groups["args"].Value;
                Assert.False(args.Contains('+') || args.Contains("$\"", StringComparison.Ordinal),
                    Path.GetFileName(file) + ": error built by concatenation: " + args);
            }
        }

        Assert.True(checkedSites > 20, "the guard found the error sites: " + checkedSites);
    }

    private static string SourceDirectory()
    {
        for (DirectoryInfo? dir = new(AppContext.BaseDirectory); dir is not null; dir = dir.Parent)
        {
            string candidate = Path.Combine(dir.FullName, "src", "Spotledger.Client.FlowDesigner");
            if (File.Exists(Path.Combine(candidate, "ProcessTextReader.cs")))
            {
                return candidate;
            }
        }

        throw new InvalidOperationException("The FlowDesigner source directory was not found above the test output.");
    }
}
