using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Analytics.Tests;

[ScreenId("fixture.invoice.form")]
internal sealed class FixtureInvoiceForm;

[ScreenId("fixture.invoice.list")]
internal sealed class FixtureInvoiceList;

public sealed class ScreenCoverageExporterTests
{
    [Fact]
    [CoversScreen("fixture.invoice.form", "empty", "error")]
    public void Exporter_lists_marked_screens_and_the_states_tests_cover()
    {
        var json = ScreenCoverageExporter.Export([typeof(ScreenCoverageExporterTests).Assembly]);
        using var doc = JsonDocument.Parse(json);
        var impl = doc.RootElement.GetProperty("implemented");
        Assert.Contains("FixtureInvoiceForm", impl.GetProperty("fixture.invoice.form").GetString(), StringComparison.Ordinal);
        Assert.True(impl.TryGetProperty("fixture.invoice.list", out _));

        var states = doc.RootElement.GetProperty("covered").GetProperty("fixture.invoice.form").GetProperty("states")
            .EnumerateArray().Select(s => s.GetString()!).ToArray();
        Assert.Equal(["empty", "error"], states);
    }

    [Fact]
    [CoversScreen("fixture.invoice.list")]
    public void A_covers_marker_without_states_covers_default()
    {
        var json = ScreenCoverageExporter.Export([typeof(ScreenCoverageExporterTests).Assembly]);
        using var doc = JsonDocument.Parse(json);
        var states = doc.RootElement.GetProperty("covered").GetProperty("fixture.invoice.list").GetProperty("states")
            .EnumerateArray().Select(s => s.GetString()!).ToArray();
        Assert.Equal(["default"], states);
    }

    [Fact]
    public void Writes_screens_found_json_when_asked()
    {
        var target = Environment.GetEnvironmentVariable("SPOT_SCREENS_FOUND");
        if (string.IsNullOrWhiteSpace(target))
        {
            return; // opt-in: CI sets it to hand the file to tools/ui-coverage/check.py --found
        }

        // the Spotledger test assemblies loaded into this run (the exporter only reads attributes)
        var assemblies = AppDomain.CurrentDomain.GetAssemblies()
            .Where(a => a.GetName().Name?.StartsWith("Spotledger.", StringComparison.Ordinal) == true);
        File.WriteAllText(target, ScreenCoverageExporter.Export(assemblies));
        Assert.True(File.Exists(target));
    }
}
