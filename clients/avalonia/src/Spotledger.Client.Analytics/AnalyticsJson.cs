using System.Globalization;
using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Analytics;

/// <summary>
/// The wire shape (docs: tools/ui-coverage/analytics-events.md). Written by hand with
/// <see cref="Utf8JsonWriter"/>: a fixed set of keys, no reflection, so a property cannot be
/// added by accident and the output is AOT safe.
/// </summary>
public static class AnalyticsJson
{
    public static string Serialize(AnalyticsPayload payload)
    {
        ArgumentNullException.ThrowIfNull(payload);
        using var stream = new MemoryStream();
        using (var w = new Utf8JsonWriter(stream))
        {
            w.WriteStartObject();
            w.WriteNumber("schema_version", payload.SchemaVersion);
            w.WriteString("install_id", payload.InstallId);
            w.WriteString("day", payload.Day.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture));
            w.WriteStartArray("counters");
            foreach (var c in payload.Counters)
            {
                w.WriteStartObject();
                w.WriteString("event", EventName(c.Event));
                if (c.ScreenId is not null)
                {
                    w.WriteString("screen_id", c.ScreenId);
                }

                if (c.ItemId is not null)
                {
                    w.WriteString(ItemKey(c.Event), c.ItemId);
                }

                w.WriteNumber("count", c.Count);
                w.WriteEndObject();
            }

            w.WriteEndArray();
            w.WriteNumber("dropped", payload.Dropped);
            w.WriteNumber("rejected", payload.Rejected);
            w.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    public static string EventName(AnalyticsEvent e) => e switch
    {
        AnalyticsEvent.ScreenOpened => "screen_opened",
        AnalyticsEvent.ActionUsed => "action_used",
        AnalyticsEvent.ErrorShown => "error_shown",
        AnalyticsEvent.ShortcutUsed => "shortcut_used",
        _ => throw new ArgumentOutOfRangeException(nameof(e)),
    };

    private static string ItemKey(AnalyticsEvent e) => e switch
    {
        AnalyticsEvent.ActionUsed => "action_id",
        AnalyticsEvent.ErrorShown => "code",
        AnalyticsEvent.ShortcutUsed => "shortcut_id",
        _ => "item_id",
    };
}
