using System.Text;
using System.Text.Json;

namespace Spotledger.Client.Browser;

/// <summary>
/// What the budget scripts (WP-UI-09.T3) read through <c>window.spotBrowser.metrics()</c>: named marks
/// in whole microseconds since the page's time origin, counters and labels. Time is an argument: the
/// clock is a delegate the host supplies (the browser's monotonic clock in production, a fake in
/// tests), so this class reads no wall clock and holds no floating point. The first mark of a name
/// wins, so a repeated event never moves a start-up figure.
/// </summary>
public sealed class BrowserMetrics
{
    /// <summary>The JSON shape version, bumped when a key is renamed or removed.</summary>
    public const int SchemaVersion = 1;

    private readonly Func<long> _nowMicros;
    private readonly SortedDictionary<string, long> _marks = new(StringComparer.Ordinal);
    private readonly SortedDictionary<string, long> _counters = new(StringComparer.Ordinal);
    private readonly SortedDictionary<string, string> _labels = new(StringComparer.Ordinal);

    /// <summary>Creates the collector.</summary>
    /// <param name="nowMicros">The monotonic clock, in whole microseconds since the page's time origin.</param>
    public BrowserMetrics(Func<long> nowMicros)
    {
        ArgumentNullException.ThrowIfNull(nowMicros);
        _nowMicros = nowMicros;
    }

    /// <summary>True once <see cref="MarkReady"/> ran.</summary>
    public bool IsReady { get; private set; }

    /// <summary>Records the current time under <paramref name="name"/>; an existing mark is kept.</summary>
    public void Mark(string name)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(name);
        _marks.TryAdd(name, _nowMicros());
    }

    /// <summary>Adds one to a counter.</summary>
    public void Count(string name)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(name);
        _counters[name] = _counters.GetValueOrDefault(name) + 1;
    }

    /// <summary>Sets a label (the active calculation backend, the build id).</summary>
    public void Label(string name, string value)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(name);
        ArgumentNullException.ThrowIfNull(value);
        _labels[name] = value;
    }

    /// <summary>The recorded time of a mark in microseconds, or null.</summary>
    public long? MarkOf(string name) => _marks.TryGetValue(name, out var value) ? value : null;

    /// <summary>The value of a counter (zero when never counted).</summary>
    public long CountOf(string name) => _counters.GetValueOrDefault(name);

    /// <summary>The label, or null.</summary>
    public string? LabelOf(string name) => _labels.GetValueOrDefault(name);

    /// <summary>Marks the shell interactive: records <c>ready</c> and flips <see cref="IsReady"/>.</summary>
    public void MarkReady()
    {
        Mark("ready");
        IsReady = true;
    }

    /// <summary>The metrics as one JSON object with sorted keys (stable between runs of the same data).</summary>
    public string ToJson()
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteNumber("schema", SchemaVersion);
            writer.WriteBoolean("ready", IsReady);
            writer.WriteStartObject("marksUs");
            foreach (var (name, value) in _marks)
            {
                writer.WriteNumber(name, value);
            }

            writer.WriteEndObject();
            writer.WriteStartObject("counters");
            foreach (var (name, value) in _counters)
            {
                writer.WriteNumber(name, value);
            }

            writer.WriteEndObject();
            writer.WriteStartObject("labels");
            foreach (var (name, value) in _labels)
            {
                writer.WriteString(name, value);
            }

            writer.WriteEndObject();
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }
}
