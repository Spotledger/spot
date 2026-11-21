using Spotledger.Client.Shell;

namespace Spotledger.Client.Assistant;

/// <summary>An entry of the chat transcript.</summary>
public abstract class TranscriptEntry : ObservableObject
{
    /// <summary>Who or what produced the entry, for the accessible name and styling.</summary>
    public abstract string Source { get; }
}

/// <summary>What the user sent, with the chips that went along.</summary>
public sealed class UserEntry(string text, IReadOnlyList<ContextChip> chips) : TranscriptEntry
{
    public override string Source => "You";

    public string Text { get; } = text;

    public IReadOnlyList<ContextChip> Chips { get; } = chips;
}

/// <summary>Assistant prose, accumulated from deltas and rendered as limited markdown.</summary>
public sealed class AssistantEntry(Func<string, bool> isRegistered) : TranscriptEntry
{
    private string _text = string.Empty;
    private IReadOnlyList<MarkdownBlock> _blocks = [];

    public override string Source => "Assistant";

    public string Text => _text;

    public IReadOnlyList<MarkdownBlock> Blocks => _blocks;

    public void Append(string delta, int maxLength = int.MaxValue)
    {
        var room = maxLength - _text.Length;
        if (room <= 0 || string.IsNullOrEmpty(delta))
        {
            return;
        }

        _text += delta.Length > room ? delta[..room] : delta;
        _blocks = SafeMarkdown.Parse(_text, isRegistered);
        OnPropertyChanged(nameof(Text));
        OnPropertyChanged(nameof(Blocks));
    }
}

/// <summary>A tool-activity timeline row; the evidence is hidden until expanded.</summary>
public sealed class ToolEntry(ToolActivity activity) : TranscriptEntry
{
    private bool _expanded;

    public override string Source => "Activity";

    public ToolActivity Activity { get; } = activity;

    public bool IsExpanded { get => _expanded; set => SetField(ref _expanded, value); }
}

/// <summary>A fact the system states, kept visually apart from assistant prose.</summary>
public sealed class FactEntry(string text) : TranscriptEntry
{
    public override string Source => "System";

    public string Text { get; } = text;
}

/// <summary>A proposed fix with its approval flow.</summary>
public sealed class FixEntry(FixFlow flow) : TranscriptEntry
{
    public override string Source => "Proposed fix";

    public FixFlow Flow { get; } = flow;
}

/// <summary>A notice from the panel itself (offline explanation, escalation result).</summary>
public sealed class NoticeEntry(string text) : TranscriptEntry
{
    public override string Source => "Notice";

    public string Text { get; } = text;
}
