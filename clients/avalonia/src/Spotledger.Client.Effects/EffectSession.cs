using System.Text;
using System.Text.Json;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects;

/// <summary>
/// One answer to one form event: the sequence the request carried, the document diff the
/// server decided, and the effects the scripts produced.
/// </summary>
/// <param name="Sequence">The event's sequence number, from <see cref="EffectSession.BeginEvent"/>.</param>
/// <param name="DiffJson">The document diff body (<c>{}</c> when there is none).</param>
/// <param name="Batch">The effect batch (empty when there is none).</param>
public sealed record EventRound(long Sequence, string DiffJson, EffectBatch Batch)
{
    /// <summary>A round with no document diff.</summary>
    public static EventRound Of(long sequence, EffectBatch batch) => new(sequence, "{}", batch);
}

/// <summary>One in-flight form event: its sequence and the local edits it was sent with.</summary>
public sealed class PendingEvent
{
    internal PendingEvent(long sequence, IReadOnlyDictionary<string, FieldValue> localValuesAtSend)
    {
        Sequence = sequence;
        LocalValuesAtSend = localValuesAtSend;
    }

    /// <summary>The sequence the round must carry back.</summary>
    public long Sequence { get; }

    /// <summary>The values of the dirty fields when the request left, by path.</summary>
    internal IReadOnlyDictionary<string, FieldValue> LocalValuesAtSend { get; }
}

/// <summary>
/// One form's event rounds: sends an event, applies the answer's diff and effects together,
/// discards a late answer for an event a newer one superseded, and keeps the edits the user
/// typed while the request was in flight.
/// </summary>
/// <remarks>
/// <para>
/// <b>The rule, in full.</b> Each event carries a sequence from <see cref="BeginEvent"/>.
/// A round whose sequence is not newer than the last applied one is discarded whole — no
/// part of a stale answer touches the form, so the document can never move backwards. A
/// round is applied in one step: the document diff first (the server's document decision),
/// then the batch's effects in order (the server's later decision wins), then exactly the
/// fields the request carried and the fields the diff or the effects named are rebased (their
/// current value becomes the saved one), and only then are the user's pending edits restored.
/// </para>
/// <para>
/// <b>Which local edits survive.</b> A field the user changed after the request left is kept
/// when the response does not name it — neither the diff's members (a table's name covers its
/// rows) nor the effects' changed paths. A field either of those names is the server's
/// answer and replaces the local edit. A kept edit stays dirty, so the next save sends it.
/// </para>
/// <para>
/// <b>A batch this client cannot read refuses the round whole.</b> An unknown protocol
/// version means this client cannot tell what the response meant, so its diff is not applied
/// either: the caller re-reads the document instead of guessing.
/// </para>
/// </remarks>
public sealed class EffectSession
{
    private static readonly Dictionary<string, FieldValue> NoSnapshot = new(StringComparer.Ordinal);

    private readonly EffectApplier _applier;
    private readonly Dictionary<long, Dictionary<string, FieldValue>> _sentWith = new();
    private long _nextSequence;

    /// <summary>Creates the session over a form state.</summary>
    /// <param name="form">The form state the rounds apply to.</param>
    /// <param name="applier">The applier to use; a default one when null.</param>
    public EffectSession(FormState form, EffectApplier? applier = null)
    {
        ArgumentNullException.ThrowIfNull(form);
        State = form;
        _applier = applier ?? new EffectApplier();
    }

    /// <summary>The form state every round applies to.</summary>
    public FormState State { get; }

    /// <summary>The newest sequence applied, or -1 before the first round.</summary>
    public long LatestAppliedSequence { get; private set; } = -1;

    /// <summary>Starts an event: takes the next sequence and remembers the local edits it leaves with.</summary>
    public PendingEvent BeginEvent()
    {
        long sequence = _nextSequence++;
        Dictionary<string, FieldValue> snapshot = LocalValues(State);
        _sentWith[sequence] = snapshot;
        return new PendingEvent(sequence, snapshot);
    }

    /// <summary>
    /// Applies one round's diff and effects. A superseded round answers
    /// <see cref="ApplyResult.Superseded"/> and changes nothing; a refused one answers
    /// <see cref="ApplyResult.Applied"/> false with the problem and changes nothing either.
    /// </summary>
    public ApplyResult ApplyRound(EventRound round)
    {
        ArgumentNullException.ThrowIfNull(round);
        if (round.Sequence <= LatestAppliedSequence)
        {
            return ApplyResult.Discarded(round.Sequence);
        }

        var pass = new ApplyResult.Builder();
        if (round.Batch.Version != EffectsProtocol.Version)
        {
            pass.Problem(EffectProblem.Of(
                EffectCodes.ProtocolVersion,
                $"Protocol version {round.Batch.Version} is not this client's {EffectsProtocol.Version}; the round is not applied."));
            return pass.Refused();
        }

        Dictionary<string, FieldValue> sent = _sentWith.TryGetValue(round.Sequence, out Dictionary<string, FieldValue>? atSend)
            ? atSend
            : NoSnapshot;
        Dictionary<string, FieldValue> pending = LocalValues(State);

        IReadOnlyList<string> members = ServerDiff.Members(round.DiffJson);
        foreach (EffectProblem problem in ServerDiff.Apply(State.Draft, round.DiffJson))
        {
            pass.Problem(problem);
        }

        pass.SeedChanged(members);
        ApplyResult applied = _applier.Apply(State, round.Batch);
        pass.Merge(applied);

        // The fields the request carried and the fields the server answered for become the
        // saved baseline: they are no longer pending. Everything else keeps its original, so
        // an edit the round did not answer for stays dirty for the next save.
        foreach (string path in sent.Keys)
        {
            Rebase(State, path);
        }

        foreach (string member in members)
        {
            Rebase(State, member);
        }

        var changed = new HashSet<string>(applied.ChangedPaths, StringComparer.Ordinal);
        foreach (string path in changed)
        {
            Rebase(State, path);
        }

        foreach ((string path, FieldValue value) in pending)
        {
            if (sent.TryGetValue(path, out FieldValue? previous) && ValuesEqual(previous, value))
            {
                // Untouched while the request was in flight.
                continue;
            }

            if (ServerDiff.Overrides(members, path) || changed.Contains(path))
            {
                // The server answered for this field: its answer wins.
                continue;
            }

            if (State.TrySetValue(path, value))
            {
                pass.Preserved(path);
            }
        }

        LatestAppliedSequence = round.Sequence;
        foreach (long stale in _sentWith.Keys.Where(sequence => sequence <= round.Sequence).ToArray())
        {
            _sentWith.Remove(stale);
        }

        return pass.Build(State);
    }

    /// <summary>Makes the field's current value its saved value (one field, not the draft).</summary>
    private static void Rebase(FormState form, string path)
    {
        if (!form.TryResolve(path, out FieldTarget target, out _))
        {
            return;
        }

        if (target.IsRow)
        {
            target.State?.MarkSaved();
            return;
        }

        if (target.Field.Kind == FieldKind.Table)
        {
            if (form.Draft.Tables.TryGetValue(target.Field.Name, out DocumentDraft.TableDraft? table))
            {
                table.MarkSaved();
            }

            return;
        }

        target.State?.MarkSaved();
    }

    private static Dictionary<string, FieldValue> LocalValues(FormState form)
    {
        var values = new Dictionary<string, FieldValue>(StringComparer.Ordinal);
        foreach (string path in form.DirtyPaths())
        {
            if (form.ValueAt(path) is { } value)
            {
                values[path] = value;
            }
        }

        return values;
    }

    /// <summary>Exact value equality: a multi-select compares element by element, a decimal by value.</summary>
    private static bool ValuesEqual(FieldValue left, FieldValue right)
    {
        if (ReferenceEquals(left, right))
        {
            return true;
        }

        return Encoding.UTF8.GetString(Encode(left)) == Encoding.UTF8.GetString(Encode(right));

        static byte[] Encode(FieldValue value)
        {
            using var stream = new MemoryStream();
            using (var writer = new Utf8JsonWriter(stream))
            {
                ValueCodec.WriteValue(writer, value);
            }

            return stream.ToArray();
        }
    }
}
