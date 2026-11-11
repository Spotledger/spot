using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Analytics.Tests;

/// <summary>
/// Property-style scans over randomly generated sessions (fixed seeds, so they are repeatable):
/// whatever mix of events and hostile ids is thrown at the collector, the JSON it would send
/// contains only the allowed keys and only identifier-shaped values.
/// </summary>
public sealed class PayloadPrivacyTests
{
    private static readonly HashSet<string> AllowedKeys =
    [
        "schema_version", "install_id", "day", "counters", "dropped", "rejected",
        "event", "screen_id", "action_id", "shortcut_id", "code", "count",
    ];

    /// <summary>The ids the collector under test knows; anything else must be dropped, never echoed.</summary>
    private static readonly KnownScreens Known =
        KnownScreens.From(Enumerable.Range(0, 20).Select(i => "id" + i + ".x-y_z"));

    private static readonly string[] ForbiddenKeyParts =
    [
        "value", "text", "user", "name", "email", "doc", "field", "title", "label", "message", "content", "ip", "host", "path",
    ];

    private static readonly string[] Hostile =
    [
        "jane@example.com", "Invoice INV-0042 for Acme Ltd", "1234 5678 9012 3456", "C:\\Users\\jane\\x.pdf", "/home/jane/x",
        "<script>", "  ", "a b", "ä.ö", "x\ny", new string('z', 200), "https://x.test/?token=1",
    ];

    [Theory]
    [InlineData(1)]
    [InlineData(7)]
    [InlineData(42)]
    [InlineData(2026)]
    public void Payload_has_only_allowed_keys_and_identifier_values(int seed)
    {
        var rng = new Random(seed);
        var analytics = new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), InstallId.Create(), Known);
        for (var i = 0; i < 400; i++)
        {
            string Pick() => rng.Next(3) == 0 ? Hostile[rng.Next(Hostile.Length)] : "id" + rng.Next(20) + ".x-y_z";
            switch (rng.Next(4))
            {
                case 0: analytics.ScreenOpened(Pick()); break;
                case 1: analytics.ActionUsed(Pick(), Pick()); break;
                case 2: analytics.ErrorShown(rng.Next(3) == 0 ? Hostile[rng.Next(Hostile.Length)] : "SP-" + rng.Next(9999)); break;
                default: analytics.ShortcutUsed(Pick(), Pick()); break;
            }
        }

        var json = analytics.InspectJson(new DateOnly(2026, 10, 23));
        using var doc = JsonDocument.Parse(json);
        var strings = new List<(string Key, string Value)>();
        Walk(doc.RootElement, null, strings);

        foreach (var (key, value) in strings)
        {
            if (key is "install_id" or "day" or "event")
            {
                continue;
            }

            Assert.True(
                key == "code" ? AnalyticsIds.IsValidCode(value) : AnalyticsIds.IsValidId(value),
                $"{key} carried a non identifier value");
        }

        Assert.DoesNotContain("@", json, StringComparison.Ordinal);
        Assert.DoesNotContain(" ", json.Replace("\"counters\"", "", StringComparison.Ordinal), StringComparison.Ordinal);
    }

    [Fact]
    public void Serialized_keys_are_an_allowlist_and_none_looks_like_user_data()
    {
        var analytics = new UsageAnalytics(new AnalyticsConsent(true), new InMemoryAnalyticsSink(), InstallId.Create(), KnownScreens.From(["a.b"]));
        analytics.ScreenOpened("a.b");
        analytics.ActionUsed("a.b", "save");
        analytics.ErrorShown("SP-1");
        analytics.ShortcutUsed("a.b", "form.save");

        using var doc = JsonDocument.Parse(analytics.InspectJson(new DateOnly(2026, 10, 23)));
        var keys = new HashSet<string>();
        CollectKeys(doc.RootElement, keys);

        Assert.Subset(AllowedKeys, keys);
        foreach (var key in keys)
        {
            // "screen_id", "action_id" and "install_id" are the identifiers the brief allows; nothing else may carry an id of a person
            var suspicious = ForbiddenKeyParts.Where(p => key.Contains(p, StringComparison.Ordinal) && key is not ("count" or "counters")).ToList();
            Assert.True(suspicious.Count == 0, $"key {key} looks like user data: {string.Join(",", suspicious)}");
        }
    }

    [Fact]
    public void Payload_model_has_no_properties_beyond_the_allowed_shape()
    {
        var names = typeof(AnalyticsCounter).GetProperties().Select(p => p.Name).Where(n => n != "EqualityContract").Order().ToArray();
        Assert.Equal(["Count", "Event", "ItemId", "ScreenId"], names);
        var payload = typeof(AnalyticsPayload).GetProperties().Select(p => p.Name).Where(n => n != "EqualityContract").Order().ToArray();
        Assert.Equal(["Counters", "Day", "Dropped", "InstallId", "IsEmpty", "Rejected", "SchemaVersion"], payload);
    }

    private static void CollectKeys(JsonElement e, HashSet<string> keys)
    {
        switch (e.ValueKind)
        {
            case JsonValueKind.Object:
                foreach (var p in e.EnumerateObject())
                {
                    keys.Add(p.Name);
                    CollectKeys(p.Value, keys);
                }

                break;
            case JsonValueKind.Array:
                foreach (var i in e.EnumerateArray())
                {
                    CollectKeys(i, keys);
                }

                break;
        }
    }

    private static void Walk(JsonElement e, string? key, List<(string, string)> strings)
    {
        switch (e.ValueKind)
        {
            case JsonValueKind.Object:
                foreach (var p in e.EnumerateObject())
                {
                    Walk(p.Value, p.Name, strings);
                }

                break;
            case JsonValueKind.Array:
                foreach (var i in e.EnumerateArray())
                {
                    Walk(i, key, strings);
                }

                break;
            case JsonValueKind.String:
                strings.Add((key ?? "", e.GetString() ?? ""));
                break;
        }
    }
}
