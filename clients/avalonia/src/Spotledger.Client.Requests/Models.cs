namespace Spotledger.Client.Requests;

/// <summary>How a record type may be created, from the metadata the server returns.</summary>
public enum EntryMode
{
    /// <summary>Anyone with the right may create the record directly.</summary>
    Free,

    /// <summary>Creating is direct, and the server may route it through approval.</summary>
    Governed,

    /// <summary>Creating happens only by starting the type's entry process. There is no bypass.</summary>
    ProcessOnly,
}

/// <summary>What the server says about creating one record type.</summary>
/// <param name="RecordType">The DocType.</param>
/// <param name="Mode">How it may be created.</param>
/// <param name="ProcessId">The entry process, for <see cref="EntryMode.ProcessOnly"/>.</param>
/// <param name="ProcessName">The entry process's display name, localised.</param>
public sealed record EntryInfo(string RecordType, EntryMode Mode, string? ProcessId, string? ProcessName);

/// <summary>The three views of My requests.</summary>
public enum RequestFilter
{
    /// <summary>A task of mine is open.</summary>
    WaitingForMe,

    /// <summary>I started it and somebody else has to act.</summary>
    WaitingForOthers,

    /// <summary>Done, cancelled or undone.</summary>
    Finished,
}

/// <summary>Where a run stands.</summary>
public enum RequestState
{
    /// <summary>Still going.</summary>
    Running,

    /// <summary>Completed.</summary>
    Finished,

    /// <summary>Cancelled.</summary>
    Cancelled,

    /// <summary>Undone.</summary>
    Undone,
}

/// <summary>One row of My requests.</summary>
/// <param name="Id">The run's id.</param>
/// <param name="Number">The number shown beside the label.</param>
/// <param name="Title">What the run is about.</param>
/// <param name="ProcessName">The process, localised.</param>
/// <param name="State">Where it stands.</param>
/// <param name="WaitingForMe">True when a task of the signed-in person is open.</param>
/// <param name="WaitingFor">The person it waits for, when it is somebody else.</param>
/// <param name="DueAt">The SLA due time, when there is one.</param>
/// <param name="StartedAt">When it was started.</param>
public sealed record RequestSummary(
    string Id,
    string Number,
    string Title,
    string ProcessName,
    RequestState State,
    bool WaitingForMe,
    string? WaitingFor,
    DateTimeOffset? DueAt,
    DateTimeOffset StartedAt);

/// <summary>The state of one step in the timeline.</summary>
public enum TimelineState
{
    /// <summary>Finished.</summary>
    Done,

    /// <summary>Open now.</summary>
    Current,

    /// <summary>Not reached.</summary>
    Upcoming,

    /// <summary>Passed over.</summary>
    Skipped,
}

/// <summary>One step of the timeline, from the record file.</summary>
public sealed record TimelineStep(string Title, TimelineState State, string? Person, DateTimeOffset? At);

/// <summary>A record the run created, for a link.</summary>
public sealed record RecordLink(string DocType, string Id, string Title);

/// <summary>A task of the run.</summary>
public sealed record TaskItem(string Id, string Title, string? Assignee, DateTimeOffset? DueAt, bool Open);

/// <summary>A comment on the run.</summary>
public sealed record CommentItem(string Author, string Text, DateTimeOffset At);

/// <summary>How far a conversation message got.</summary>
public enum DeliveryState
{
    /// <summary>Held, not yet sent.</summary>
    Queued,

    /// <summary>Handed to the channel.</summary>
    Sent,

    /// <summary>Received by the peer.</summary>
    Delivered,

    /// <summary>Read by the peer.</summary>
    Read,

    /// <summary>The channel refused it.</summary>
    Failed,
}

/// <summary>What is wrong with a conversation, when something is.</summary>
public enum ConversationCondition
{
    /// <summary>Nothing.</summary>
    None,

    /// <summary>The peer has not answered for longer than expected.</summary>
    Stuck,

    /// <summary>The peer cannot be reached.</summary>
    Unreachable,
}

/// <summary>One message in a conversation.</summary>
/// <param name="Outgoing">True for a message we sent.</param>
/// <param name="Text">The message.</param>
/// <param name="At">When.</param>
/// <param name="Delivery">How far it got (for an outgoing message).</param>
public sealed record ConversationMessage(bool Outgoing, string Text, DateTimeOffset At, DeliveryState Delivery);

/// <summary>A conversation with a peer (another organisation or system) about the run.</summary>
/// <param name="Peer">The peer's name.</param>
/// <param name="Messages">Oldest first.</param>
/// <param name="Condition">What is wrong, if anything.</param>
/// <param name="Since">Since when the condition holds.</param>
public sealed record ConversationInfo(
    string Peer,
    IReadOnlyList<ConversationMessage> Messages,
    ConversationCondition Condition,
    DateTimeOffset? Since);

/// <summary>Whether an action on the run is allowed, and if not, why not.</summary>
/// <param name="Allowed">True when the server allows it.</param>
/// <param name="Reason">The reason in the person's words, when it is not allowed.</param>
/// <param name="Blockers">What stands in the way.</param>
public sealed record ActionState(bool Allowed, string? Reason, IReadOnlyList<string> Blockers)
{
    /// <summary>The action is allowed.</summary>
    public static ActionState Ok { get; } = new(true, null, []);
}

/// <summary>Everything the request page shows.</summary>
public sealed record RequestDetail(
    RequestSummary Summary,
    IReadOnlyList<TimelineStep> Timeline,
    IReadOnlyList<RecordLink> Records,
    IReadOnlyList<TaskItem> Tasks,
    IReadOnlyList<CommentItem> Comments,
    IReadOnlyList<ConversationInfo> Conversations,
    ActionState Undo,
    ActionState Cancel);

/// <summary>A form step as the server describes it.</summary>
/// <param name="TaskId">The task that holds the step.</param>
/// <param name="RequestId">The run.</param>
/// <param name="Title">The step's title.</param>
/// <param name="Instructions">What the person should do.</param>
/// <param name="WaitingFor">Who the run is waiting for, when it is not the signed-in person.</param>
/// <param name="RecordType">The draft record's DocType.</param>
/// <param name="RecordId">The draft record's id.</param>
public sealed record StepInfo(
    string TaskId,
    string RequestId,
    string Title,
    string? Instructions,
    string? WaitingFor,
    string RecordType,
    string RecordId);

/// <summary>A run that was just started.</summary>
/// <param name="RequestId">The run.</param>
/// <param name="FirstStep">The first form step, when the person has to fill it in now.</param>
public sealed record StartedRequest(string RequestId, StepInfo? FirstStep);
