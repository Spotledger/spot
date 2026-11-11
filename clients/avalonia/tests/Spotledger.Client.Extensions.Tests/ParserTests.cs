using System.Text;
using Spotledger.Client.Expressions;
using Spotledger.Client.Extensions;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The resolved document the server sends: what the model keeps, what it drops with
/// a diagnostic, and the version, size and shape refusals that disable a set whole.</summary>
public sealed class ParserTests
{
    [Fact]
    public void SampleResolvedDocumentKeepsEveryItemOfItsSlots()
    {
        ExtensionSet set = Sample.Set();
        Assert.True(set.IsEnabled);
        Assert.False(set.UpgradeRequired);
        Assert.Empty(set.Diagnostics);
        Assert.Equal(ExtensionLimits.ContractVersion, set.ContractVersion);
        Assert.Equal(2, set.Actions.Count);
        Assert.Single(set.Panels);
        Assert.Single(set.Components);
        Assert.Single(set.Tiles);
        Assert.Empty(set.Tabs);
        Assert.Equal(["doc.sidepanel", "doc.toolbar", "home.tiles"], set.PopulatedSlots);
        Assert.Equal("sample.start_work", set.Actions[0].Id);
        Assert.IsType<CallEffect>(set.Actions[0].Effect);
        Assert.IsType<OpenEffect>(set.Actions[1].Effect);
        PanelSpec panel = set.Panels[0];
        Assert.Equal(4, panel.Blocks.Count);
        Assert.IsType<FieldListBlock>(panel.Blocks[0]);
        Assert.IsType<StatBlock>(panel.Blocks[1]);
        Assert.IsType<TableBlock>(panel.Blocks[2]);
        Assert.IsType<TextPanelBlock>(panel.Blocks[3]);
    }

    [Fact]
    public void ItemsAreInDependencyThenOrderThenAppThenIdOrder()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "actions": [
            { "id": "sample.second", "origin": "app:sample", "label": "B", "slot": "shell.statusbar", "order": 20, "navigate_url": "/sample/reports" },
            { "id": "sample.first", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "order": 10, "navigate_url": "/sample/reports" }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Facts());
        Assert.Empty(set.Diagnostics);
        using ExtensionContext context = ExtensionContext.FromValues(new Dictionary<string, FieldValue>(), Repo.Now);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.ShellStatusbar, context);
        Assert.Equal(["sample.first", "sample.second"], view.Entries.Select(entry => entry.Id));
    }

    [Fact]
    public void ANewerContractMajorDisablesTheWholeSetWithOneDiagnostic()
    {
        ExtensionSet set = ExtensionSet.Parse("""{ "contract_version": "1.0.0", "requires_shell": "0.1.0", "app": "sample" }""");
        Assert.False(set.IsEnabled);
        ExtensionDiagnostic diagnostic = Assert.Single(set.Diagnostics);
        Assert.Equal(ExtensionCodes.UnsupportedContract, diagnostic.Code);
        Assert.Empty(set.Actions);
    }

    [Fact]
    public void AnOlderShellKeepsNothingAndAsksForAnUpgrade()
    {
        ExtensionSet set = ExtensionSet.Parse("""{ "contract_version": "0.1.0", "requires_shell": "9.9.9", "app": "sample" }""");
        Assert.False(set.IsEnabled);
        Assert.True(set.UpgradeRequired);
        Assert.Equal("9.9.9", set.RequiresShell);
        Assert.Equal(ExtensionCodes.UnsupportedContract, Assert.Single(set.Diagnostics).Code);
    }

    [Fact]
    public void AMalformedDocumentReportsAndNeverThrows()
    {
        ExtensionSet set = ExtensionSet.Parse("{ \"contract_version\": ");
        Assert.False(set.IsEnabled);
        Assert.Equal(ExtensionCodes.Schema, Assert.Single(set.Diagnostics).Code);
        Assert.False(ExtensionSet.Parse("[]").IsEnabled);
        Assert.False(ExtensionSet.Parse("""{ "app": "sample" }""").IsEnabled);
    }

    [Fact]
    public void AnOversizedDocumentIsRejectedBeforeItsItemsAreRead()
    {
        string huge = "{\"contract_version\":\"0.1.0\",\"requires_shell\":\"0.1.0\",\"app\":\"sample\",\"$comment\":\""
            + new string('x', ExtensionLimits.MaxDocumentBytes) + "\"}";
        ExtensionSet set = ExtensionSet.Parse(huge, Facts());
        Assert.False(set.IsEnabled);
        Assert.Equal(ExtensionCodes.DocumentTooLarge, Assert.Single(set.Diagnostics).Code);
    }

    [Fact]
    public void ClosedObjectsAreRefusedAndTheItemIsDropped()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "actions": [
            { "id": "sample.ok", "origin": "app:sample", "label": "Ok", "slot": "shell.statusbar", "navigate_url": "/sample/reports" },
            { "id": "sample.bad", "origin": "app:sample", "label": "Bad", "slot": "shell.statusbar", "navigate_url": "/sample/reports", "onclick": "alert(1)" }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Facts());
        Assert.Equal(["sample.ok"], set.Actions.Select(action => action.Id));
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.Schema && diagnostic.Id.EndsWith("sample.bad", StringComparison.Ordinal));
    }

    [Fact]
    public void StructuralRulesAreEnforcedAgainOnTheClient()
    {
        Assert.Equal(ExtensionCodes.UnknownSlot, FirstCode(Slot("doc.footer")));
        Assert.Equal(ExtensionCodes.SlotKindMismatch, FirstCode(Slot("doc.sidepanel")));
        Assert.Equal(ExtensionCodes.DoctypeRequired, FirstCode(Slot("doc.toolbar")));
        Assert.Equal(ExtensionCodes.UnknownIcon, FirstCode(
            """{ "id": "sample.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "icon": "http://x/y.svg", "navigate_url": "/sample/reports" }"""));
        Assert.Equal(ExtensionCodes.LabelTooLong, FirstCode(
            """{ "id": "sample.a", "origin": "app:sample", "label": "A label that is far too long for a status bar slot", "slot": "shell.statusbar", "navigate_url": "/sample/reports" }"""));
        Assert.Equal(ExtensionCodes.EffectInvalid, FirstCode(
            """{ "id": "sample.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar" }"""));
        Assert.Equal(ExtensionCodes.EffectInvalid, FirstCode(
            """{ "id": "sample.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "navigate_url": "/sample/reports", "open": { "destination": "sample.projects" } }"""));
        Assert.Equal(ExtensionCodes.ExternalUrl, FirstCode(
            """{ "id": "sample.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "navigate_url": "https://example.com/x" }"""));
        Assert.Equal(ExtensionCodes.OriginMismatch, FirstCode(
            """{ "id": "other.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "navigate_url": "/sample/reports" }"""));
    }

    [Fact]
    public void TwoItemsWithOneIdKeepTheFirstAndReportTheSecond()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "actions": [
            { "id": "sample.a", "origin": "app:sample", "label": "First", "slot": "shell.statusbar", "order": 1, "navigate_url": "/sample/reports" },
            { "id": "sample.a", "origin": "app:sample", "label": "Second", "slot": "shell.statusbar", "order": 2, "navigate_url": "/sample/reports" }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Facts());
        Assert.Equal("First", Assert.Single(set.Actions).Label);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.DuplicateId);
    }

    [Fact]
    public void ABrokenExpressionHidesItsItemAndReportsOnce()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "actions": [
            { "id": "sample.a", "origin": "app:sample", "label": "A", "slot": "shell.statusbar", "visible_if": "doc.status ==", "navigate_url": "/sample/reports" }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Facts());
        Assert.Single(set.Actions);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.ExpressionInvalid);
        using ExtensionContext context = ExtensionContext.FromValues(new Dictionary<string, FieldValue>(), Repo.Now);
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.ShellStatusbar, context);
        Assert.Empty(view.Entries);
        view.Refresh();
        view.Refresh();
        Assert.Single(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.ExpressionInvalid);
    }

    private static string? FirstCode(string action)
    {
        string document = $$"""
        { "contract_version": "0.1.0", "requires_shell": "0.1.0", "app": "sample", "actions": [ {{action}} ] }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Facts());
        return set.Diagnostics.Count > 0 ? set.Diagnostics[0].Code : null;
    }

    private static string Slot(string slot)
    {
        string doctype = slot.StartsWith("doc.", StringComparison.Ordinal) && slot != "doc.toolbar" ? "\"doctype\": \"task\", " : string.Empty;
        return "{ \"id\": \"sample.a\", \"origin\": \"app:sample\", \"label\": \"A\", \"slot\": \"" + slot + "\", " + doctype + "\"navigate_url\": \"/sample/reports\" }";
    }

    private static ExtensionFacts Facts() => Sample.Facts;
}

/// <summary>The shared corpus: every invalid file the server refuses must be refused here
/// with the same code, without an exception, and the rest of its document must still render.</summary>
public sealed class CorpusTests
{
    [Fact]
    public void EveryInvalidCorpusFileYieldsItsExpectedCodeAndNeverThrows()
    {
        IReadOnlyList<(string File, string Code)> files = Corpus.InvalidFiles();
        Assert.Equal(32, files.Count);
        List<string> failures = [];
        foreach ((string file, string code) in files)
        {
            ExtensionSet set = ExtensionSet.Parse(File.ReadAllText(file), Sample.Facts, Path.GetFileName(file));
            bool found = set.Diagnostics.Any(diagnostic => diagnostic.Code == code);
            if (!found)
            {
                failures.Add($"{Path.GetFileName(file)}: expected {code}, got [{string.Join(", ", set.Diagnostics.Select(diagnostic => diagnostic.Code).Distinct())}]");
            }
        }
        Assert.True(failures.Count == 0, string.Join(Environment.NewLine, failures));
    }

    [Fact]
    public void TheRestOfADocumentStillRendersWhenOneItemIsBroken()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "actions": [
            { "id": "sample.start_work", "origin": "app:sample", "label": "Start work", "slot": "doc.toolbar", "doctype": "task", "permission": "task:write",
              "call": { "doctype": "task", "method": "start_work" } }
          ],
          "panels": [
            { "id": "sample.broken", "origin": "app:sample", "label": "Broken", "slot": "doc.sidepanel", "doctype": "project",
              "blocks": [ { "type": "text", "markdown": "<script>alert(1)</script>" } ] }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        Assert.Single(set.Actions);
        Assert.Empty(set.Panels);
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.RawHtml);
        using ExtensionContext context = Sample.TaskContext();
        using SlotView toolbar = set.ForSlot(ExtensionSlotCatalog.DocToolbar, context, "task");
        Assert.Single(toolbar.Entries);
    }

    [Fact]
    public void AnUnknownControlKeepsAVisiblePlaceholderInsteadOfVanishing()
    {
        const string document = """
        {
          "contract_version": "0.1.0",
          "requires_shell": "0.1.0",
          "app": "sample",
          "components": [
            { "id": "sample.c", "origin": "app:sample", "props": [],
              "body": { "node": "column", "children": [
                { "node": "control", "control": "iframe" },
                { "node": "control", "control": "label", "text": "still here" } ] } }
          ]
        }
        """;
        ExtensionSet set = ExtensionSet.Parse(document, Sample.Facts);
        ComponentSpec component = Assert.Single(set.Components);
        using ExtensionContext context = ExtensionContext.FromValues(new Dictionary<string, FieldValue>(), Repo.Now);
        ComponentModel expanded = ComponentModel.Expand(component, new Dictionary<string, ExpressionValue>(), context, set, _ => null);
        Assert.True(expanded.IsUsable);
        Assert.Contains(expanded.Root!.Children, node => node.Kind == ExpandedNodeKind.Placeholder && node.Placeholder == "iframe");
        Assert.Contains(expanded.Root.Children, node => node.Kind == ExpandedNodeKind.Label && node.Text == "still here");
        Assert.Contains(set.Diagnostics, diagnostic => diagnostic.Code == ExtensionCodes.Schema);
    }
}

/// <summary>Hostile input: random mutations of the sample document and random bytes must
/// never throw out of the reader.</summary>
public sealed class FuzzTests
{
    [Fact]
    public void RandomMutationsOfTheSampleDocumentNeverThrow()
    {
        string original = Sample.ResolvedJson;
        Random random = new(20261021);
        for (int iteration = 0; iteration < 400; iteration++)
        {
            string mutated = Mutate(original, random);
            ExtensionSet set = ExtensionSet.Parse(mutated, Sample.Facts, "fuzz.json");
            Assert.NotNull(set);
        }
    }

    [Fact]
    public void RandomBytesNeverThrow()
    {
        Random random = new(7);
        for (int iteration = 0; iteration < 200; iteration++)
        {
            StringBuilder text = new();
            int length = random.Next(0, 512);
            for (int index = 0; index < length; index++)
            {
                text.Append((char)random.Next(1, 0x2FFF));
            }
            ExtensionSet set = ExtensionSet.Parse(text.ToString());
            Assert.NotNull(set);
        }
    }

    [Fact]
    public void DeeplyNestedJsonIsRefusedWithoutAStackOverflow()
    {
        string deep = "{\"contract_version\":\"0.1.0\",\"app\":\"sample\",\"components\":[" + new string('[', 2000) + new string(']', 2000) + "]}";
        ExtensionSet set = ExtensionSet.Parse(deep, Sample.Facts);
        Assert.False(set.IsEnabled);
        Assert.NotEmpty(set.Diagnostics);
    }

    private static string Mutate(string source, Random random)
    {
        char[] buffer = source.ToCharArray();
        int edits = random.Next(1, 6);
        for (int edit = 0; edit < edits; edit++)
        {
            switch (random.Next(4))
            {
                case 0 when buffer.Length > 0:
                    buffer[random.Next(buffer.Length)] = (char)random.Next(32, 127);
                    break;
                case 1:
                    buffer = [.. buffer[..Math.Min(buffer.Length, random.Next(buffer.Length + 1))]];
                    break;
                case 2:
                    {
                        string insertion = random.Next(2) == 0 ? "\"x\":" : "{}[]\",:";
                        int at = random.Next(buffer.Length + 1);
                        buffer = [.. buffer[..at], .. insertion.ToCharArray(), .. buffer[at..]];
                        break;
                    }
                default:
                    if (buffer.Length > 2)
                    {
                        int at = random.Next(buffer.Length - 1);
                        (buffer[at], buffer[at + 1]) = (buffer[at + 1], buffer[at]);
                    }
                    break;
            }
        }
        return new string(buffer);
    }
}
