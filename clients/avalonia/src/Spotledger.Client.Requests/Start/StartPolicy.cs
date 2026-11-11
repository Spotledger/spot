using Spotledger.Client.Api;

namespace Spotledger.Client.Requests;

/// <summary>Which button creates a record.</summary>
public enum CreateKind
{
    /// <summary>The plain "New" button.</summary>
    New,

    /// <summary>"Start &lt;process&gt;": the record type is process-only.</summary>
    Start,
}

/// <summary>The create button of a list or the home page.</summary>
/// <param name="Kind">New or Start.</param>
/// <param name="Label">The button text.</param>
/// <param name="Gesture">The keyboard shortcut. It is the same for both kinds.</param>
/// <param name="ProcessId">The process to start, for <see cref="CreateKind.Start"/>.</param>
public sealed record CreateAction(CreateKind Kind, string Label, string Gesture, string? ProcessId);

/// <summary>Where the server's <c>bpm_entry.start_process</c> refusal sends the person.</summary>
/// <param name="ProcessId">The process to start.</param>
/// <param name="Message">A sentence in the person's words, never the raw error.</param>
public sealed record StartRoute(string ProcessId, string Message);

/// <summary>Decides the create button and routes the "start the process instead" refusal.</summary>
public static class StartPolicy
{
    /// <summary>The shortcut of the create button, unchanged by the mode.</summary>
    public const string CreateGesture = "Ctrl+N";

    /// <summary>The server's refusal code for a direct create of a process-only type.</summary>
    public const string StartProcessCode = "bpm_entry.start_process";

    /// <summary>The create button for a record type: "Start X" when it is process-only and an entry
    /// process is known, otherwise "New".</summary>
    public static CreateAction CreateActionFor(EntryInfo info, RequestText text)
    {
        ArgumentNullException.ThrowIfNull(info);
        ArgumentNullException.ThrowIfNull(text);
        if (info.Mode == EntryMode.ProcessOnly && !string.IsNullOrWhiteSpace(info.ProcessId))
        {
            var name = string.IsNullOrWhiteSpace(info.ProcessName) ? info.RecordType : info.ProcessName;
            return new CreateAction(CreateKind.Start, text.Get("start.action", ("process", name)), CreateGesture, info.ProcessId);
        }

        return new CreateAction(CreateKind.New, text.Get("new.action"), CreateGesture, null);
    }

    /// <summary>When <paramref name="error"/> is the start-process refusal, the route to Start; otherwise null.
    /// The process id is the <c>process</c> field of the error when the server sends it, else the
    /// entry process <paramref name="info"/> names; with neither there is no route.</summary>
    public static StartRoute? RouteFor(ApiError error, EntryInfo? info, RequestText text)
    {
        ArgumentNullException.ThrowIfNull(error);
        ArgumentNullException.ThrowIfNull(text);
        if (!string.Equals(error.Code, StartProcessCode, StringComparison.Ordinal))
        {
            return null;
        }

        var process = error.FieldErrors
            .Where(f => string.Equals(f.Field, "process", StringComparison.Ordinal) && !string.IsNullOrWhiteSpace(f.Message))
            .Select(f => f.Message)
            .FirstOrDefault() ?? info?.ProcessId;
        if (string.IsNullOrWhiteSpace(process))
        {
            return null;
        }

        var name = !string.IsNullOrWhiteSpace(info?.ProcessName) ? info.ProcessName : (info?.RecordType ?? process);
        return new StartRoute(process, text.Get("start.routed", ("process", name)));
    }
}
