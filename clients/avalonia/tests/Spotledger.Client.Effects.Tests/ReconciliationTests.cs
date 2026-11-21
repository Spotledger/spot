using System.Globalization;
using System.Text.Json;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// One form's event rounds: a late answer for a superseded event, a local edit made while a
/// request is in flight, and the server's diff and effects deciding the same field. The rule
/// these tests pin is the one documented on <see cref="EffectSession"/>: a round is applied
/// whole or not at all, the diff and the effects are the server's answer and win over a local
/// edit they name, and every other local edit survives and stays dirty.
/// </summary>
public class ReconciliationTests
{
    private static readonly string[] QtyOnly = { "qty" };
    private static readonly string[] DiffMembers = { "id", "version", "_state", "unknown_field", "title" };

    /// <summary>A late answer for an event a newer one superseded changes nothing.</summary>
    [Fact]
    public void ALateAnswerForASupersededEventIsDiscarded()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent older = session.BeginEvent();
        PendingEvent newer = session.BeginEvent();

        ApplyResult applied = session.ApplyRound(new EventRound(
            newer.Sequence,
            """{"title":"newer"}""",
            TestForm.Batch("""{"kind":"set_value","path":"qty","value":9}""")));
        Assert.False(applied.Superseded);
        Assert.Equal("newer", TestForm.Text(form, "title"));
        Assert.Equal("9", TestForm.Text(form, "qty"));

        ApplyResult late = session.ApplyRound(new EventRound(
            older.Sequence,
            """{"title":"older"}""",
            TestForm.Batch("""{"kind":"set_value","path":"qty","value":1}""")));

        Assert.True(late.Superseded);
        Assert.False(late.Applied);
        Assert.Equal(older.Sequence, late.SupersededSequence);
        Assert.Equal(newer.Sequence, session.LatestAppliedSequence);
        Assert.Equal("newer", TestForm.Text(form, "title"));
        Assert.Equal("9", TestForm.Text(form, "qty"));
        Assert.Empty(late.ChangedPaths);
    }

    /// <summary>The same sequence twice is discarded the second time.</summary>
    [Fact]
    public void TheSameSequenceIsAppliedOnce()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();
        EventRound round = new(pending.Sequence, """{"title":"first"}""", EffectBatch.Empty);

        Assert.False(session.ApplyRound(round).Superseded);
        ApplyResult again = session.ApplyRound(round);

        Assert.True(again.Superseded);
        Assert.Equal("first", TestForm.Text(form, "title"));
    }

    /// <summary>A local edit to a field the response does not name survives the round and stays dirty.</summary>
    [Fact]
    public void AnInFlightEditSurvivesWhenTheServerDoesNotNameIt()
    {
        FormState form = TestForm.Load();
        DocumentDraft draft = form.Draft;
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        // The user types while the request is in flight.
        draft.Set("qty", new IntFieldValue(7));

        ApplyResult result = session.ApplyRound(new EventRound(
            pending.Sequence,
            """{"title":"server title"}""",
            EffectBatch.Empty));

        Assert.False(result.HasProblems);
        Assert.Equal("7", TestForm.Text(form, "qty"));
        Assert.Equal("server title", TestForm.Text(form, "title"));
        Assert.Equal(QtyOnly, result.PreservedLocalEdits);
        Assert.Contains("qty", draft.DirtyPaths());
        Assert.DoesNotContain("title", draft.DirtyPaths());

        // The preserved edit is what the next save would send.
        using JsonDocument patch = JsonDocument.Parse(draft.ToPatch());
        Assert.Equal(QtyOnly, patch.RootElement.EnumerateObject().Select(property => property.Name));
    }

    /// <summary>The server diff wins over a local edit to the field it names.</summary>
    [Fact]
    public void TheServerDiffOverridesALocalEditOfTheSameField()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        form.Draft.Set("title", new TextFieldValue("mine"));

        ApplyResult result = session.ApplyRound(new EventRound(
            pending.Sequence,
            """{"title":"theirs"}""",
            EffectBatch.Empty));

        Assert.Equal("theirs", TestForm.Text(form, "title"));
        Assert.Empty(result.PreservedLocalEdits);
        Assert.False(form.Draft.IsDirty);
    }

    /// <summary>An effect's write is also the server's answer: it wins over a local edit.</summary>
    [Fact]
    public void AnEffectOverridesALocalEditOfTheSameField()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        form.Draft.Set("qty", new IntFieldValue(7));

        ApplyResult result = session.ApplyRound(new EventRound(
            pending.Sequence,
            "{}",
            TestForm.Batch("""{"kind":"set_value","path":"qty","value":9}""")));

        Assert.Equal("9", TestForm.Text(form, "qty"));
        Assert.Empty(result.PreservedLocalEdits);
        Assert.False(form.Draft.IsDirty);
    }

    /// <summary>A table diff replaces its rows, so a row-level local edit is the server's to
    /// decide; a top-level local edit in the same round still survives.</summary>
    [Fact]
    public void ATableDiffReplacesRowsAndKeepsOtherEdits()
    {
        FormState form = TestForm.Load(withRow: true);
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        Assert.True(form.TrySetValue("items[1].note", new TextFieldValue("local note")));
        form.Draft.Set("qty", new IntFieldValue(3));

        const string Diff = """
            {
              "items": [
                { "id": "01900000-0000-7000-8000-0000000000b1", "qty": 1, "unit_price": "10.00", "line_total": "10.00", "note": "server note" }
              ]
            }
            """;
        ApplyResult result = session.ApplyRound(new EventRound(pending.Sequence, Diff, EffectBatch.Empty));

        Assert.False(result.HasProblems);
        Assert.Equal("server note", TestForm.Text(form, "items[1].note"));
        Assert.Equal(QtyOnly, result.PreservedLocalEdits);
        Assert.Equal("3", TestForm.Text(form, "qty"));
        Assert.Single(form.Draft.Table("items").Rows);
    }

    /// <summary>A batch of a version the client does not speak refuses the round whole: its diff
    /// is not applied either, the sequence is not advanced, and a corrected round still applies.</summary>
    [Fact]
    public void AnUnreadableBatchRefusesTheWholeRound()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        var newer = new EffectBatch(2, Array.AsReadOnly(new Effect[] { new SetValueEffect("qty", "5") }));
        ApplyResult refused = session.ApplyRound(new EventRound(pending.Sequence, """{"title":"guess"}""", newer));

        Assert.False(refused.Applied);
        Assert.False(refused.Superseded);
        Assert.Equal(EffectCodes.ProtocolVersion, Assert.Single(refused.Problems).Code);
        Assert.Equal("first", TestForm.Text(form, "title"));
        Assert.Equal("2", TestForm.Text(form, "qty"));
        Assert.Equal(-1, session.LatestAppliedSequence);

        ApplyResult retried = session.ApplyRound(new EventRound(pending.Sequence, """{"title":"now"}""", EffectBatch.Empty));
        Assert.True(retried.Applied);
        Assert.Equal("now", TestForm.Text(form, "title"));
    }

    /// <summary>An edit made before the request left but not changed since is not "in flight": the
    /// round rebases the draft, so it is no longer dirty.</summary>
    [Fact]
    public void AnEditSentWithTheRequestIsRebasedByTheRound()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        form.Draft.Set("qty", new IntFieldValue(4));
        PendingEvent pending = session.BeginEvent();

        ApplyResult result = session.ApplyRound(new EventRound(pending.Sequence, "{}", EffectBatch.Empty));

        Assert.Empty(result.PreservedLocalEdits);
        Assert.False(form.Draft.IsDirty);
        Assert.Equal("4", TestForm.Text(form, "qty"));
    }

    /// <summary>A round applied without <see cref="EffectSession.BeginEvent"/> still works, and
    /// treats every pending edit as made in flight.</summary>
    [Fact]
    public void ARoundWithoutABeginStillApplies()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        form.Draft.Set("qty", new IntFieldValue(6));

        ApplyResult result = session.ApplyRound(new EventRound(0, """{"title":"server"}""", EffectBatch.Empty));

        Assert.True(result.Applied);
        Assert.Equal(QtyOnly, result.PreservedLocalEdits);
        Assert.Equal("6", TestForm.Text(form, "qty"));
    }

    /// <summary>Round intents are per round: clearing them leaves values and presentation alone.</summary>
    [Fact]
    public void IntentsClearPerRoundAndValuesPersist()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();
        string batch = """
            {"kind":"batch","effects":[
              {"kind":"show_message","level":"info","text":"Saved"},
              {"kind":"open_dialog","dialog_id":"pick_customer"},
              {"kind":"focus","path":"qty"},
              {"kind":"set_value","path":"title","value":"kept"}
            ]}
            """;

        session.ApplyRound(new EventRound(pending.Sequence, "{}", TestForm.Batch(batch)));
        Assert.Single(form.Messages);
        Assert.Single(form.Dialogs);
        Assert.Equal("qty", form.Focus);

        form.ClearIntents();

        Assert.Empty(form.Messages);
        Assert.Empty(form.Dialogs);
        Assert.Null(form.Focus);
        Assert.Equal("kept", TestForm.Text(form, "title"));
    }

    /// <summary>The diff's standard members are not values: the id and version never appear in
    /// the form's fields, and an unknown member is ignored rather than fatal.</summary>
    [Fact]
    public void TheDiffSkipsStandardAndUnknownMembers()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        ApplyResult result = session.ApplyRound(new EventRound(
            pending.Sequence,
            """{"id":"01900000-0000-7000-8000-0000000000aa","version":3,"_state":"draft","unknown_field":"x","title":"kept"}""",
            EffectBatch.Empty));

        Assert.False(result.HasProblems);
        Assert.Equal("kept", TestForm.Text(form, "title"));
        Assert.Null(form.Draft.Model.Field("id"));
        Assert.Equal(DiffMembers, ServerDiff.Members(
            """{"id":"x","version":3,"_state":"draft","unknown_field":"x","title":"kept"}"""));
    }

    /// <summary>A diff that is not an object is reported, not thrown, and the effects still apply.</summary>
    [Fact]
    public void AMalformedDiffIsADiagnosticAndTheEffectsStillApply()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        ApplyResult result = session.ApplyRound(new EventRound(
            pending.Sequence,
            "[1,2,3]",
            TestForm.Batch("""{"kind":"set_value","path":"title","value":"applied"}""")));

        Assert.True(result.Applied);
        Assert.Equal(EffectCodes.EffectInvalid, Assert.Single(result.Problems).Code);
        Assert.Equal("applied", TestForm.Text(form, "title"));
    }

    /// <summary>A decimal a diff carries stays exact: the diff is a value, not a formatting hint.</summary>
    [Fact]
    public void ADiffValueStaysExact()
    {
        FormState form = TestForm.Load();
        var session = new EffectSession(form);
        PendingEvent pending = session.BeginEvent();

        session.ApplyRound(new EventRound(pending.Sequence, """{"amount":"30.00"}""", EffectBatch.Empty));

        Assert.Equal("30.00", TestForm.Text(form, "amount"));
        Assert.Equal(
            "30.00",
            ((DecimalFieldValue)form.ValueAt("amount")!).CanonicalText);
    }

    /// <summary>Nothing in a round reads a clock: two identical sessions end in the same state.</summary>
    [Fact]
    public void TwoIdenticalRoundsEndTheSame()
    {
        static string Run()
        {
            FormState form = TestForm.Load(withRow: true);
            var session = new EffectSession(form);
            PendingEvent pending = session.BeginEvent();
            session.ApplyRound(new EventRound(
                pending.Sequence,
                """{"title":"server","items":[{"id":"01900000-0000-7000-8000-0000000000b1","qty":2,"unit_price":"10.00","line_total":"20.00","note":"row"}]}""",
                TestForm.Batch("""{"kind":"set_label","path":"qty","label":"Quantity"}""")));
            return string.Create(
                CultureInfo.InvariantCulture,
                $"{TestForm.Text(form, "title")}|{TestForm.Text(form, "items[1].line_total")}|{form.Presentation.LabelFor("qty")}");
        }

        Assert.Equal(Run(), Run());
    }
}
