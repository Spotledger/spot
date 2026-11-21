using System.Reflection;
using System.Text.Json;
using System.Text.RegularExpressions;
using Spotledger.Client.Extensions;
using Spotledger.Client.Extensions.Renderer;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The client's mirrored tables and the contract's schemas must not drift: the
/// slot list, the icon set, the component and panel limits, and the resolved golden the
/// tests read. A contract change that is not mirrored fails here first.</summary>
public sealed partial class ContractParityTests
{
    [Fact]
    public void TheSlotTableMatchesTheSchema()
    {
        using JsonDocument schema = JsonDocument.Parse(File.ReadAllText(Path.Combine(Repo.UiExt, "schema", "slot.json")));
        JsonElement slots = schema.RootElement.GetProperty("x-slots");
        Assert.Equal(slots.EnumerateObject().Count(), ExtensionSlotCatalog.All.Count);
        foreach (JsonProperty slot in slots.EnumerateObject())
        {
            SlotSpec spec = Assert.IsType<SlotSpec>(ExtensionSlotCatalog.Find(slot.Name));
            JsonElement body = slot.Value;
            string accepts = body.GetProperty("accepts").GetString()!;
            Assert.Equal(accepts, KindName(spec.Accepts));
            Assert.Equal(body.GetProperty("max_per_app").GetInt32(), spec.MaxPerApp);
            Assert.Equal(body.GetProperty("label_max").GetInt32(), spec.LabelMax);
        }
    }

    [Fact]
    public void TheIconSetMatchesTheSchema()
    {
        using JsonDocument schema = JsonDocument.Parse(File.ReadAllText(Path.Combine(Repo.UiExt, "schema", "common.json")));
        string[] icons = [.. schema.RootElement.GetProperty("$defs").GetProperty("icon").GetProperty("enum").EnumerateArray().Select(item => item.GetString()!)];
        Assert.Equal(icons.Order(StringComparer.Ordinal), ExtensionLimits.IconTokens.Order(StringComparer.Ordinal));
    }

    [Fact]
    public void TheComponentAndPanelLimitsMatchTheSchemas()
    {
        using JsonDocument component = JsonDocument.Parse(File.ReadAllText(Path.Combine(Repo.UiExt, "schema", "component.json")));
        JsonElement limits = component.RootElement.GetProperty("x-limits");
        Assert.Equal(ExtensionLimits.MaxComponentDepth, limits.GetProperty("max_depth").GetInt32());
        Assert.Equal(ExtensionLimits.MaxComponentNodes, limits.GetProperty("max_nodes").GetInt32());
        Assert.Equal(ExtensionLimits.MaxRepeat, limits.GetProperty("max_repeat").GetInt32());
        using JsonDocument panel = JsonDocument.Parse(File.ReadAllText(Path.Combine(Repo.UiExt, "schema", "panel.json")));
        JsonElement blocks = panel.RootElement.GetProperty("$defs").GetProperty("block");
        JsonElement first = blocks.GetProperty("oneOf")[0];
        Assert.Equal(ExtensionLimits.MaxListEntries, first.GetProperty("properties").GetProperty("fields").GetProperty("maxItems").GetInt32());
        Assert.Equal(ExtensionLimits.MaxPanelBlocks, panel.RootElement.GetProperty("$defs").GetProperty("panel").GetProperty("properties").GetProperty("blocks").GetProperty("maxItems").GetInt32());
        Assert.Equal(ExtensionLimits.MaxTableColumns, 8);
        Assert.Equal(ExtensionLimits.MaxTableRows, 50);
    }

    [Fact]
    public void TheCopiedResolvedGoldenIsTheServersGolden()
    {
        using JsonDocument server = JsonDocument.Parse(File.ReadAllText(Repo.GoldenResolved));
        using JsonDocument copy = JsonDocument.Parse(File.ReadAllText(Repo.Data("sample-app.resolved.json")));
        Assert.True(JsonElement.DeepEquals(server.RootElement, copy.RootElement),
            "Data/sample-app.resolved.json must stay byte-for-byte the golden the Rust resolver produced");
    }

    [Fact]
    public void TheModelAssemblyReferencesNoUiFrameworkSoABrowserHeadCanReuseIt()
    {
        Assembly model = typeof(ExtensionSet).Assembly;
        string[] references = [.. model.GetReferencedAssemblies().Select(name => name.Name ?? string.Empty)];
        Assert.DoesNotContain(references, name => name.StartsWith("Avalonia", StringComparison.Ordinal));
        Assert.DoesNotContain(references, name => name.StartsWith("System.Drawing", StringComparison.Ordinal));
        // The renderer is the only project with Avalonia, and it is a separate assembly.
        Assert.StartsWith("Avalonia", typeof(ActionBar).Assembly.GetReferencedAssemblies()
            .Select(name => name.Name ?? string.Empty)
            .First(name => name.StartsWith("Avalonia", StringComparison.Ordinal)), StringComparison.Ordinal);
    }

    [Fact]
    public void TheRendererHasNoHardCodedColourSizeOrFont()
    {
        string directory = Path.Combine(Repo.Root, "clients", "avalonia", "src", "Spotledger.Client.Extensions", "Renderer");
        List<string> offences = [];
        foreach (string file in Directory.GetFiles(directory, "*.cs"))
        {
            string text = File.ReadAllText(file);
            foreach (Match match in HexColour().Matches(text))
            {
                offences.Add($"{Path.GetFileName(file)}: hex colour {match.Value}");
            }
            foreach (Match match in BrushesMember().Matches(text))
            {
                offences.Add($"{Path.GetFileName(file)}: literal brush {match.Value}");
            }
            foreach (Match match in NumericFontSize().Matches(text))
            {
                offences.Add($"{Path.GetFileName(file)}: literal font size {match.Value}");
            }
        }
        Assert.True(offences.Count == 0, string.Join(Environment.NewLine, offences));
    }

    [Fact]
    public void TheTargetsCheckDocumentNamesTheBrowserGap()
    {
        string path = Path.Combine(Repo.Root, "clients", "avalonia", "tests", "Spotledger.Client.Extensions.Tests", "targets-check.md");
        Assert.True(File.Exists(path), "the browser-target check document exists");
        string text = File.ReadAllText(path);
        Assert.Contains("net10.0", text, StringComparison.Ordinal);
        Assert.Contains("browser", text, StringComparison.OrdinalIgnoreCase);
        Assert.Contains("CI", text, StringComparison.Ordinal);
    }

    [GeneratedRegex(@"#[0-9A-Fa-f]{6}\b", RegexOptions.CultureInvariant)]
    private static partial Regex HexColour();

    [GeneratedRegex(@"Brushes\.[A-Za-z]+", RegexOptions.CultureInvariant)]
    private static partial Regex BrushesMember();

    [GeneratedRegex(@"FontSize\s*=\s*[0-9]", RegexOptions.CultureInvariant)]
    private static partial Regex NumericFontSize();

    private static string KindName(ExtensionItemKind kind) => kind switch
    {
        ExtensionItemKind.Action => "action",
        ExtensionItemKind.Panel => "panel",
        ExtensionItemKind.Tile => "tile",
        _ => "tab"
    };
}
