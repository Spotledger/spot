using System.Text;
using System.Text.Json;
using System.Text.RegularExpressions;
using Spotledger.Client.Realtime;

namespace Spotledger.Client.Notifications;

/// <summary>How serious a notice is. Shown by colour token and by shape, never by colour alone.</summary>
public enum NoticeSeverity
{
    /// <summary>Information.</summary>
    Info = 0,

    /// <summary>Something went well.</summary>
    Success = 1,

    /// <summary>Something needs attention.</summary>
    Warning = 2,

    /// <summary>Something failed.</summary>
    Error = 3,
}

/// <summary>A reference to a document: never a URL.</summary>
/// <param name="DocType">The DocType name.</param>
/// <param name="Id">The document id.</param>
public sealed record NoticeLink(string DocType, string Id);

/// <summary>One notice, already reduced to bounded plain text.</summary>
/// <param name="EventId">The server's id for the committed event; redelivery carries the same one.</param>
/// <param name="Title">One line of plain text.</param>
/// <param name="Body">Plain text, possibly empty.</param>
/// <param name="Severity">How serious it is.</param>
/// <param name="Link">The document it points to, if any.</param>
/// <param name="ReceivedAt">When the server published it.</param>
public sealed record Notice(
    string EventId,
    string Title,
    string Body,
    NoticeSeverity Severity,
    NoticeLink? Link,
    DateTimeOffset ReceivedAt);

/// <summary>
/// Reads a <c>notification</c> event of the <c>user:&lt;id&gt;</c> room (contract 0.8.0:
/// <c>{event_id, event: "user_notice", data}</c>). Everything in <c>data</c> is untrusted text: control
/// and bidirectional-override characters are removed, lengths are bounded, and the link is a
/// (doctype, id) pair that matches a strict name pattern, never a URL.
/// </summary>
public static partial class NoticeParser
{
    /// <summary>The longest title kept, in characters.</summary>
    public const int MaxTitle = 120;

    /// <summary>The longest body kept, in characters.</summary>
    public const int MaxBody = 500;

    private const int MaxEventId = 128;

    /// <summary>Parses one realtime event; false when it is not a notice this client understands.</summary>
    public static bool TryParse(RealtimeEvent realtimeEvent, out Notice notice)
    {
        ArgumentNullException.ThrowIfNull(realtimeEvent);
        notice = null!;
        if (realtimeEvent.Kind != RealtimeEventKind.Notification || realtimeEvent.Payload.ValueKind != JsonValueKind.Object)
        {
            return false;
        }

        var payload = realtimeEvent.Payload;
        if (Text(payload, "event") != "user_notice" || Text(payload, "event_id") is not { Length: > 0 and <= MaxEventId } eventId)
        {
            return false;
        }

        string? title = null, body = null, severity = null, doctype = null, id = null;
        if (payload.TryGetProperty("data", out var data) && data.ValueKind == JsonValueKind.Object)
        {
            title = Text(data, "title");
            body = Text(data, "body");
            severity = Text(data, "severity");
            doctype = Text(data, "doctype");
            id = Text(data, "id");
        }

        var link = doctype is not null && id is not null && NamePattern().IsMatch(doctype) && IdPattern().IsMatch(id)
            ? new NoticeLink(doctype, id)
            : null;
        var cleanTitle = Clean(title, MaxTitle);
        notice = new Notice(
            eventId,
            cleanTitle.Length > 0 ? cleanTitle : "Notification",
            Clean(body, MaxBody),
            severity?.ToLowerInvariant() switch
            {
                "success" => NoticeSeverity.Success,
                "warning" => NoticeSeverity.Warning,
                "error" => NoticeSeverity.Error,
                _ => NoticeSeverity.Info,
            },
            link,
            realtimeEvent.PublishedAt);
        return true;
    }

    /// <summary>
    /// Reduces untrusted text to one bounded line: controls and bidirectional overrides become
    /// spaces or vanish, runs of spaces collapse, and a cut never splits a surrogate pair.
    /// </summary>
    public static string Clean(string? text, int maxChars)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        var builder = new StringBuilder(Math.Min(text.Length, maxChars + 1));
        var lastSpace = true;
        var truncated = false;
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (IsBidiControl(c) || c == '​' || c == '﻿')
            {
                continue;
            }

            if (char.IsControl(c) || char.IsWhiteSpace(c))
            {
                if (!lastSpace)
                {
                    builder.Append(' ');
                    lastSpace = true;
                }

                continue;
            }

            if (builder.Length >= maxChars)
            {
                truncated = true;
                break;
            }

            builder.Append(c);
            lastSpace = false;
        }

        if (builder.Length > 0 && char.IsHighSurrogate(builder[^1]))
        {
            builder.Length--;
        }

        var result = builder.ToString().TrimEnd();
        return truncated ? result + "…" : result;
    }

    private static bool IsBidiControl(char c) =>
        c is >= '‪' and <= '‮' or >= '⁦' and <= '⁩' or '‎' or '‏' or '؜';

    private static string? Text(JsonElement element, string name) =>
        element.TryGetProperty(name, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;

    [GeneratedRegex(@"^[A-Za-z0-9][A-Za-z0-9 _\-.]{0,139}$")]
    private static partial Regex NamePattern();

    [GeneratedRegex(@"^[A-Za-z0-9][A-Za-z0-9 _\-.:/#@]{0,139}$")]
    private static partial Regex IdPattern();
}
