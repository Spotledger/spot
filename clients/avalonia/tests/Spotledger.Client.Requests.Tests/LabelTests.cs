using System.Text.RegularExpressions;
using Xunit;

namespace Spotledger.Client.Requests.Tests;

public sealed class LabelTests
{
    [Fact]
    public void The_shipped_default_label_comes_from_the_catalogue()
    {
        var label = new RunLabelResolver().Resolve("en");

        Assert.Equal(new RunLabel("Request", "Requests"), label);
    }

    [Fact]
    public void An_organisation_override_shows_Application_everywhere()
    {
        var text = Texts.English("Application", "Applications");

        Assert.Equal("My Applications", text.Get("mine.title"));
        Assert.Equal("Application 7", text.Get("mine.reference", ("number", "7")));
        Assert.Equal("Cancel Application", text.Get("page.cancel"));
        Assert.Equal("No Applications here.", text.Get("mine.empty"));
    }

    [Fact]
    public void Another_organisation_keeps_the_default()
    {
        var catalogue = new MemoryRunLabelCatalogue()
            .Set(RunLabel.SingularKey, "en", "acme", "Application")
            .Set(RunLabel.PluralKey, "en", "acme", "Applications");

        Assert.Equal("Request", new RunLabelResolver(catalogue).Resolve("en", "globex").Singular);
    }

    [Fact]
    public void A_regional_locale_falls_back_to_its_language()
    {
        Assert.Equal("طلب", new RunLabelResolver().Resolve("ar-EG").Singular);
    }

    [Fact]
    public void An_unknown_locale_falls_back_to_English()
    {
        Assert.Equal("Request", new RunLabelResolver().Resolve("xx").Singular);
    }

    [Theory]
    [InlineData("ar", true)]
    [InlineData("ar-EG", true)]
    [InlineData("he", true)]
    [InlineData("en", false)]
    [InlineData("de-AT", false)]
    public void Right_to_left_languages_flow_right_to_left(string locale, bool rtl)
    {
        var text = Texts.For(locale);

        Assert.Equal(rtl, text.RightToLeft);
        Assert.Equal(rtl ? Avalonia.Media.FlowDirection.RightToLeft : Avalonia.Media.FlowDirection.LeftToRight, text.FlowDirection);
    }

    [Fact]
    public void A_missing_translation_falls_back_to_English_text_with_the_local_label()
    {
        var text = Texts.For("ar");

        Assert.Equal("حفظ", text.Get("step.save"));
        Assert.Equal("Waiting to be sent", text.Get("delivery.queued"));
        Assert.Contains("طلب", text.Get("page.cancel"), StringComparison.Ordinal);
    }

    [Fact]
    public void No_resource_file_spells_the_engine_word_and_texts_never_spell_the_default_label()
    {
        var directory = Path.Combine(AppContext.BaseDirectory, "Resources");
        var files = Directory.GetFiles(directory, "*.json");
        Assert.NotEmpty(files);
        var engineWord = new Regex(@"\bcases?\b", RegexOptions.IgnoreCase);

        foreach (var file in files)
        {
            var content = File.ReadAllText(file);
            Assert.False(engineWord.IsMatch(content), file + " spells the engine word");
            if (Path.GetFileName(file).StartsWith("texts.", StringComparison.Ordinal))
            {
                Assert.DoesNotContain("Request", content, StringComparison.OrdinalIgnoreCase);
            }
        }
    }

    [Fact]
    public void No_source_file_spells_the_engine_word_or_the_default_label_in_a_string()
    {
        var root = FindSourceRoot();
        var literal = new Regex("\"[^\"\\n]*\\b(cases?|requests?)\\b[^\"\\n]*\"", RegexOptions.IgnoreCase);
        foreach (var file in Directory.GetFiles(root, "*.cs", SearchOption.AllDirectories)
                     .Where(f => !f.Contains(Path.DirectorySeparatorChar + "obj" + Path.DirectorySeparatorChar, StringComparison.Ordinal)
                                 && !f.Contains(Path.DirectorySeparatorChar + "bin" + Path.DirectorySeparatorChar, StringComparison.Ordinal)))
        {
            foreach (var line in File.ReadLines(file))
            {
                var trimmed = line.TrimStart();
                if (trimmed.StartsWith("///", StringComparison.Ordinal) || trimmed.StartsWith("//", StringComparison.Ordinal))
                {
                    continue;
                }

                // Error codes and the catalogue keys are identifiers, not screen text.
                var match = literal.Match(line.Replace("Spotledger.Client.Requests", "Spotledger.Client.R", StringComparison.Ordinal));
                Assert.False(
                    match.Success && !match.Value.Contains("bpm_entry", StringComparison.Ordinal) && !match.Value.Contains("bpm.run", StringComparison.Ordinal),
                    $"{Path.GetFileName(file)}: {line.Trim()}");
            }
        }
    }

    private static string FindSourceRoot()
    {
        var directory = new DirectoryInfo(AppContext.BaseDirectory);
        while (directory is not null && !Directory.Exists(Path.Combine(directory.FullName, "src", "Spotledger.Client.Requests")))
        {
            directory = directory.Parent;
        }

        return Path.Combine(directory?.FullName ?? throw new DirectoryNotFoundException("The source tree was not found."), "src", "Spotledger.Client.Requests");
    }
}
