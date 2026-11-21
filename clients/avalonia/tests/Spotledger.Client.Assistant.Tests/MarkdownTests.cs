using Xunit;

namespace Spotledger.Client.Assistant.Tests;

public sealed class MarkdownTests
{
    private static bool Registered(string d) => d.StartsWith("spot://doc/", StringComparison.Ordinal);

    public static TheoryData<string> HostileInputs() => new()
    {
        "<script>alert(1)</script>",
        "<img src=x onerror=alert(1)>",
        "![tracker](https://evil.example/pixel.png)",
        "[click](javascript:alert(1))",
        "[click](https://evil.example)",
        "[click](data:text/html;base64,AAAA)",
        "[click](spot://other/1)",
        "<a href=\"https://evil.example\">x</a>",
        "[a](spot://doc/../../etc)\u0000",
    };

    [Theory]
    [MemberData(nameof(HostileInputs))]
    public void HostileInputNeverYieldsALinkToAnUnregisteredTarget(string input)
    {
        var blocks = SafeMarkdown.Parse(input, Registered);
        foreach (var run in blocks.SelectMany(b => b.Runs).Where(r => r.Kind == RunKind.Link))
        {
            Assert.True(Registered(run.Destination!), run.Destination);
        }
    }

    [Fact]
    public void HtmlStaysLiteralText()
    {
        var blocks = SafeMarkdown.Parse("<b>hi</b>", Registered);
        var run = Assert.Single(Assert.Single(blocks).Runs);
        Assert.Equal(RunKind.Text, run.Kind);
        Assert.Equal("<b>hi</b>", run.Text);
    }

    [Fact]
    public void ImagesBecomeTheirAltTextAndNeverARunWithATarget()
    {
        var blocks = SafeMarkdown.Parse("![chart](https://evil.example/x.png)", Registered);
        Assert.All(blocks.SelectMany(b => b.Runs), r => Assert.Null(r.Destination));
        Assert.Equal("chart", SafeMarkdown.ToPlainText(blocks));
    }

    [Fact]
    public void RegisteredLinksBoldCodeAndBulletsSurvive()
    {
        var blocks = SafeMarkdown.Parse("Open [job 17](spot://doc/job/17) and **retry** `job.retry`\n- first\n- second", Registered);
        Assert.Equal(3, blocks.Count);
        Assert.Contains(blocks[0].Runs, r => r.Kind == RunKind.Link && r.Destination == "spot://doc/job/17");
        Assert.Contains(blocks[0].Runs, r => r.Kind == RunKind.Bold && r.Text == "retry");
        Assert.Contains(blocks[0].Runs, r => r.Kind == RunKind.Code);
        Assert.True(blocks[1].IsBullet && blocks[2].IsBullet);
    }

    [Fact]
    public void UnterminatedMarkupIsText()
    {
        var blocks = SafeMarkdown.Parse("**bold [x](y `code", Registered);
        Assert.All(blocks.SelectMany(b => b.Runs), r => Assert.Equal(RunKind.Text, r.Kind));
    }

    [Theory]
    [InlineData("password=hunter2", "hunter2")]
    [InlineData("Authorization: Bearer abc.def.ghi", "abc.def.ghi")]
    [InlineData("mail bob@example.com now", "bob@example.com")]
    [InlineData("key AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "AAAAAAAAAAAAAAAA")]
    public void RedactorRemovesSecrets(string input, string secret)
    {
        var result = DiagnosticRedactor.Redact(input);
        Assert.DoesNotContain(secret, result, StringComparison.Ordinal);
        Assert.Contains(DiagnosticRedactor.Marker, result, StringComparison.Ordinal);
    }

    [Fact]
    public void OfflineCatalogueHasAFallbackForUnknownCodes()
    {
        Assert.Contains("no built-in explanation", OfflineCatalogue.BuiltIn.Explain("nope"), StringComparison.Ordinal);
        Assert.NotNull(OfflineCatalogue.BuiltIn.Find("cache.stale"));
    }
}
