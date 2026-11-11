// The sample app guest in C# (componentize-dotnet), WP-STR-07.T1.
//
// The world is wit/world.wit (a copy of crates/framework/spot-wasm/wit/test-world.wit). The
// bindings are generated into generated/wit/TestWorld.cs: the namespace TestWorldWorld, the
// export interface ITestWorldWorldExports (one method, OnEvent), and the error record
// ITestWorldWorldImports.Error. The world's `result<string, error>` is returned as the method's
// value on `ok`, and raised as a WitException on `err`.
//
// The behaviour is the sample app reduced to one DocType, one lifecycle step and one
// script-callable function. Hours are parsed to hundredths and formatted back in integer
// arithmetic (ADR-12: no floating point for quantities).

using System.Globalization;

namespace TestWorldWorld;

public class TestWorldWorldExportsImpl : ITestWorldWorldExports
{
    // The bookable hours range, in hundredths (0.25 to 24.00).
    private const int HoursMin = 25;
    private const int HoursMax = 2400;

    /// <summary>The world's export `on-event`.</summary>
    public static string OnEvent(string @event)
    {
        string op = JsonField(@event, "op");
        string name = JsonField(@event, "name");
        if (op == "step" && name == "sample:check_hours")
        {
            return CheckHours(JsonField(@event, "hours"));
        }
        if (op == "call" && name == "sample:double_hours")
        {
            return DoubleHours(JsonField(@event, "hours"));
        }
        throw Refusal("sample.unknown_op", $"no operation {op}:{name}");
    }

    private static string CheckHours(string hours)
    {
        if (!TryHundredths(hours, out int value))
        {
            throw Refusal("sample.bad_hours", "hours must be a decimal string");
        }
        if (value < HoursMin || value > HoursMax)
        {
            throw Refusal("sample.hours_out_of_range", "Hours must be between 0.25 and 24.");
        }
        return "{\"ok\":true,\"step\":\"sample:check_hours\"}";
    }

    private static string DoubleHours(string hours)
    {
        if (!TryHundredths(hours, out int value))
        {
            throw Refusal("sample.bad_hours", "hours must be a decimal string");
        }
        return $"{{\"value\":\"{FormatHours(value * 2)}\"}}";
    }

    // The world's error record, as a WitException the generated export catches.
    private static WitException Refusal(string code, string message) =>
        new WitException<ITestWorldWorldImports.Error>(
            new ITestWorldWorldImports.Error(code, message), 0);

    // Parses a non-negative decimal with at most two places into hundredths.
    private static bool TryHundredths(string text, out int hundredths)
    {
        hundredths = 0;
        string[] parts = text.Split('.');
        if (parts.Length > 2 || parts[0].Length == 0 || !parts[0].All(char.IsAsciiDigit))
        {
            return false;
        }
        if (!int.TryParse(parts[0], NumberStyles.None, CultureInfo.InvariantCulture, out int whole))
        {
            return false;
        }
        string fraction = parts.Length == 2 ? parts[1] : string.Empty;
        if (fraction.Length > 2 || !fraction.All(char.IsAsciiDigit))
        {
            return false;
        }
        int scaled = fraction.Length == 0
            ? 0
            : int.Parse(fraction.PadRight(2, '0'), CultureInfo.InvariantCulture);
        hundredths = whole * 100 + scaled;
        return true;
    }

    // Formats hundredths as a canonical two-place decimal.
    private static string FormatHours(int hundredths) =>
        string.Create(CultureInfo.InvariantCulture, $"{hundredths / 100}.{hundredths % 100:00}");

    // Reads a JSON string field's value: "key":"value". The envelope carries only string values.
    private static string JsonField(string json, string key)
    {
        string needle = $"\"{key}\":\"";
        int start = json.IndexOf(needle, StringComparison.Ordinal);
        if (start < 0)
        {
            return string.Empty;
        }
        int valueStart = start + needle.Length;
        int end = json.IndexOf('"', valueStart);
        return end < 0 ? string.Empty : json[valueStart..end];
    }
}
