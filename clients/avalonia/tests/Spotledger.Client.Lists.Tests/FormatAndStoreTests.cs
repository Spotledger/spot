using System.Globalization;
using System.Text.Json;
using Spotledger.Client.Grid;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

public sealed class FormatAndStoreTests
{
    private static ListColumn Column(string name)
        => ListColumnPlanner.Build([(Fixtures.Model().Field(name)!, (double?)null)], Fixtures.Permissions().DocTypes["order"])[0];

    private static string Format(string field, string json, string culture = "en-US", IReadOnlyDictionary<string, string>? labels = null)
        => new ListCellFormatter(CultureInfo.GetCultureInfo(culture), TimeZoneInfo.Utc)
            .Format(Column(field), JsonDocument.Parse(json).RootElement, labels).Text;

    [Theory]
    [InlineData("en-US", "\"1234567.5\"", "1,234,567.50")]
    [InlineData("de-DE", "\"1234567.5\"", "1.234.567,50")]
    [InlineData("en-US", "\"-12.345\"", "-12.345")]
    [InlineData("en-US", "\"0\"", "0.00")]
    [InlineData("en-US", "\"12345678901234567890123456789.01\"", "12,345,678,901,234,567,890,123,456,789.01")]
    public void DecimalsKeepEveryDigitAndFollowTheCulture(string culture, string json, string expected)
        => Assert.Equal(expected, Format("amount", json, culture));

    [Fact]
    public void DecimalsAreRightAlignedAndDatesFollowTheUsersLocale()
    {
        var cell = new ListCellFormatter(CultureInfo.GetCultureInfo("de-DE"), TimeZoneInfo.Utc)
            .Format(Column("amount"), JsonDocument.Parse("\"1.5\"").RootElement, null);

        Assert.Equal(GridCellAlignment.End, cell.Alignment);
        Assert.Equal("23.10.2026", Format("due", "\"2026-10-23\"", "de-DE"));
        Assert.Equal("10/23/2026", Format("due", "\"2026-10-23\"", "en-US"));
        Assert.Equal("10/23/2026 10:30 AM", Format("stamp", "\"2026-10-23T10:30:00Z\"", "en-US").Replace('\u202f', ' '));
        Assert.Equal("1,200", Format("qty", "1200"));
    }

    [Fact]
    public void BoolsLinksAndUnknownShapesAreShownAsPlainData()
    {
        Assert.Equal("Yes", Format("paid", "true"));
        Assert.Equal("No", Format("paid", "false"));
        Assert.Equal("Acme", Format("customer", "\"p-1\"", labels: new Dictionary<string, string> { ["p-1"] = "Acme" }));
        Assert.Equal("p-2", Format("customer", "\"p-2\"", labels: new Dictionary<string, string>()));
        Assert.Equal("not-a-date", Format("due", "\"not-a-date\""));
        Assert.Equal(string.Empty, Format("title", "null"));
    }

    [Fact]
    public void TextIsDataNotMarkupAndControlCharactersAreDefused()
    {
        Assert.Equal("<b>bold</b> {Binding Path=x} [link](http://x)", Format("title", "\"<b>bold</b> {Binding Path=x} [link](http://x)\""));
        Assert.Equal("a b c", Format("title", "\"a\\nb\\u0007c\""));
        Assert.Equal(500, Format("title", JsonSerializer.Serialize(new string('x', 5000))).Length);
    }

    [Fact]
    public async Task FileStoreRoundTripsAndKeepsPeopleAndDocTypesApart()
    {
        var dir = Path.Combine(Path.GetTempPath(), "lists-" + Guid.NewGuid().ToString("N"));
        try
        {
            var store = new FileFilterStore(dir);
            var prefs = new ListPreferences([new ColumnPreference("title", 140)], [new SavedFilter("Mine", [new FilterCondition("title", FilterOperator.Contains, ["a"])], "q")]);

            await store.SaveAsync("ana@example.test", "order", prefs, TestContext.Current.CancellationToken);

            var back = await store.LoadAsync("ana@example.test", "order", TestContext.Current.CancellationToken);
            Assert.Equal("title", back.Columns![0].Field);
            Assert.Equal(140, back.Columns[0].Width);
            Assert.Equal(FilterOperator.Contains, back.SavedFilters[0].Conditions[0].Operator);
            Assert.Equal("q", back.SavedFilters[0].Search);
            Assert.Same(ListPreferences.Empty, await store.LoadAsync("bob@example.test", "order", TestContext.Current.CancellationToken));
            Assert.Same(ListPreferences.Empty, await store.LoadAsync("ana@example.test", "party", TestContext.Current.CancellationToken));
            Assert.DoesNotContain(Directory.EnumerateFiles(dir, "*", SearchOption.AllDirectories), f => f.Contains("ana", StringComparison.Ordinal));
            Assert.DoesNotContain(Directory.EnumerateFiles(dir, "*.tmp", SearchOption.AllDirectories), _ => true);
        }
        finally
        {
            if (Directory.Exists(dir)) Directory.Delete(dir, true);
        }
    }

    [Fact]
    public async Task FileStoreRefusesPathTricksAndSurvivesCorruptOrHugeFiles()
    {
        var dir = Path.Combine(Path.GetTempPath(), "lists-" + Guid.NewGuid().ToString("N"));
        try
        {
            var store = new FileFilterStore(dir);
            await Assert.ThrowsAsync<ArgumentException>(() => store.SaveAsync("ana", "../evil", ListPreferences.Empty, TestContext.Current.CancellationToken));
            await store.SaveAsync("ana", "order", ListPreferences.Empty, TestContext.Current.CancellationToken);
            var file = Directory.EnumerateFiles(dir, "order.json", SearchOption.AllDirectories).Single();

            await File.WriteAllTextAsync(file, "{ not json", TestContext.Current.CancellationToken);
            Assert.Same(ListPreferences.Empty, await store.LoadAsync("ana", "order", TestContext.Current.CancellationToken));
            await File.WriteAllTextAsync(file, new string('x', ListPreferences.MaxBytes + 1), TestContext.Current.CancellationToken);
            Assert.Same(ListPreferences.Empty, await store.LoadAsync("ana", "order", TestContext.Current.CancellationToken));
        }
        finally
        {
            if (Directory.Exists(dir)) Directory.Delete(dir, true);
        }
    }

    [Fact]
    public void PreferencesAreNormalisedOnTheWayIn()
    {
        var json = """{"columns":[{"field":"title","width":99999},{"field":"Bad Name"},{"field":"title"}],"savedFilters":[{"name":"","conditions":[]},{"name":"x","conditions":[{"field":"nope!","operator":"Contains","values":["a"]}]}]}""";

        var prefs = ListPreferences.FromJson(json);

        Assert.Equal(["title"], prefs.Columns!.Select(c => c.Field));
        Assert.Equal(2000, prefs.Columns![0].Width);
        Assert.Single(prefs.SavedFilters);
        Assert.Empty(prefs.SavedFilters[0].Conditions);
        Assert.Same(ListPreferences.Empty, ListPreferences.FromJson("""{"savedFilters":[{"name":"x","conditions":[{"field":"title","operator":"Nope","values":[]}]}]}"""));
    }
}
