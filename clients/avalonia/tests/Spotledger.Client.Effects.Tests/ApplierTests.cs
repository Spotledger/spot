using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// The applier's behaviour beyond the corpus: what each effect does to the metadata core's
/// state, what it refuses with, and the promise that a bad effect costs nothing but its own
/// diagnostic. The corpus pins the state a whole round produces; these tests pin the rules
/// the corpus cannot show one case at a time.
/// </summary>
public class ApplierTests
{
    private static readonly EffectApplier Applier = new();

    private static readonly string[] RowTotalPath = { "items[1].line_total" };
    private static readonly string[] OptionValues = { "draft", "open" };
    private static readonly string[] ActionsAfterAdd = { "pick", "other" };
    private static readonly string[] ActionsAfterRemove = { "other" };
    private static readonly string[] KindsOfThree = { "set_value", "set_label", "set_value" };
    private static readonly string[] KindsNested = { "set_label", "set_label", "set_value", "set_label" };
    private static readonly string[] KindsOfTwo = { "set_label", "set_label" };

    // ------------------------------------------------------------------------------------------
    // set_value

    /// <summary>An int lands in the draft and the draft says it is dirty.</summary>
    [Fact]
    public void SetValueWritesTheDraftAndMarksItDirty()
    {
        FormState form = TestForm.Load();
        int raised = 0;
        form.ValueApplied += (_, change) =>
        {
            Assert.Equal("qty", change.Path);
            Assert.Equal("set_value", change.Kind);
            raised++;
        };

        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"qty","value":5}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("5", TestForm.Text(form, "qty"));
        Assert.True(form.Draft.IsDirty);
        Assert.Contains("qty", result.DirtyPaths);
        Assert.Equal(1, raised);
    }

    /// <summary>A decimal keeps its exact wire text, scale included, and never a float.</summary>
    [Fact]
    public void ADecimalStaysTheExactWireString()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"amount","value":"12.50"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("12.50", TestForm.Text(form, "amount"));
        Assert.Equal("12.50", ((DecimalFieldValue)form.ValueAt("amount")!).CanonicalText);
    }

    /// <summary>A decimal with more places than the field allows is refused, never rounded.</summary>
    [Fact]
    public void ADecimalWithTooManyPlacesIsRefusedNotFounded()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"amount","value":"1.234"}"""));

        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.ValueInvalid && problem.Path == "amount");
        Assert.Equal("20.00", TestForm.Text(form, "amount"));
        Assert.False(form.Draft.IsDirty);
    }

    /// <summary>A value of the wrong wire shape is refused: a bool is not "yes" and a text is
    /// not a number.</summary>
    [Fact]
    public void AValueOfTheWrongWireShapeIsRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_value","path":"active","value":"yes"}""",
            """{"kind":"set_value","path":"title","value":5}"""));

        Assert.Equal(2, result.Problems.Count);
        Assert.All(result.Problems, problem => Assert.Equal(EffectCodes.ValueInvalid, problem.Code));
        Assert.Equal("true", TestForm.Text(form, "active"));
        Assert.Equal("first", TestForm.Text(form, "title"));
    }

    /// <summary>A select refuses a value it does not list.</summary>
    [Fact]
    public void ASelectRefusesAnUnlistedValue()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"status","value":"archived"}"""));

        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.ValueInvalid);
        Assert.Equal("draft", TestForm.Text(form, "status"));
    }

    /// <summary>Text longer than the field's limit is refused.</summary>
    [Fact]
    public void TextLongerThanTheLimitIsRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"title","value":"this title is far too long"}"""));

        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.ValueInvalid);
        Assert.Equal("first", TestForm.Text(form, "title"));
    }

    /// <summary>An unknown field is dropped with a problem naming the path the server wrote.</summary>
    [Fact]
    public void AnUnknownPathIsDroppedWithThePathNamed()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"nope","value":1}"""));

        EffectProblem problem = Assert.Single(result.Problems);
        Assert.Equal(EffectCodes.PathUnknown, problem.Code);
        Assert.Equal("nope", problem.Path);
        Assert.Empty(result.ChangedPaths);
    }

    /// <summary>A row column path writes exactly one cell, and only that cell is dirty.</summary>
    [Fact]
    public void ARowColumnPathWritesOneCell()
    {
        FormState form = TestForm.Load(withRow: true);
        ApplyResult result = Applier.Apply(
            form,
            TestForm.Batch("""{"kind":"set_value","path":"items[1].line_total","value":"11.00"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("11.00", TestForm.Text(form, "items[1].line_total"));
        Assert.Equal(RowTotalPath, result.DirtyPaths);
        Assert.Equal("10.00", TestForm.Text(form, "items[1].unit_price"));
    }

    /// <summary>A row the document does not have is an unknown path.</summary>
    [Fact]
    public void ARowTheDocumentDoesNotHaveIsUnknown()
    {
        FormState form = TestForm.Load(withRow: true);
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"items[9].qty","value":1}"""));

        Assert.Equal(EffectCodes.PathUnknown, Assert.Single(result.Problems).Code);
        Assert.Equal("items[9].qty", result.Problems[0].Path);
    }

    /// <summary>A child table is never set by one effect.</summary>
    [Fact]
    public void AChildTableIsNotSetByOneEffect()
    {
        FormState form = TestForm.Load(withRow: true);
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"items","value":[]}"""));

        Assert.Equal(EffectCodes.ValueInvalid, Assert.Single(result.Problems).Code);
    }

    /// <summary>A field the DocType declares read-only cannot be written by an effect.</summary>
    [Fact]
    public void ADeclaredReadOnlyFieldCannotBeWritten()
    {
        DocTypeModel model = TestForm.Model();
        var fields = model.Fields.Select(field => field.Name == "rate"
            ? new FieldModel(name: "rate", label: "rate", kind: FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true)
            : field).ToList();
        var draft = new DocumentDraft(new DocTypeModel("order", "Order", 1, false, fields, model.ChildTables));
        draft.LoadFrom("""{"id":"01900000-0000-7000-8000-0000000000aa","rate":"10.00","qty":2}""");
        var form = new FormState(draft);

        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"rate","value":"11.00"}"""));

        Assert.Equal(EffectCodes.FieldNotWritable, Assert.Single(result.Problems).Code);
        Assert.Equal("10.00", TestForm.Text(form, "rate"));
    }

    /// <summary>A field an earlier effect made read-only in the same batch cannot be written.</summary>
    [Fact]
    public void AnEffectThatLockedAFieldStopsALaterWrite()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_read_only","path":"qty","read_only":true}""",
            """{"kind":"set_value","path":"qty","value":7}"""));

        Assert.Equal(EffectCodes.FieldNotWritable, Assert.Single(result.Problems).Code);
        Assert.Equal("2", TestForm.Text(form, "qty"));
        Assert.True(form.IsReadOnly("qty"));
    }

    // ------------------------------------------------------------------------------------------
    // toggles

    /// <summary>The three flags set, clear and last-wins, and a renderer sees them.</summary>
    [Fact]
    public void FlagsToggleAndTheLastOneWins()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_read_only","path":"note","read_only":true}""",
            """{"kind":"set_hidden","path":"note","hidden":true}""",
            """{"kind":"set_required","path":"qty","required":true}""",
            """{"kind":"set_read_only","path":"note","read_only":false}"""));

        Assert.False(result.HasProblems);
        Assert.False(form.IsReadOnly("note"));
        Assert.True(form.IsHidden("note"));
        Assert.True(form.IsRequired("qty"));
        Assert.Contains("note", form.Presentation.HiddenPaths);
        Assert.Empty(form.Presentation.ReadOnlyPaths);
    }

    /// <summary>A flag on an unknown field is dropped, not applied to nothing silently.</summary>
    [Fact]
    public void AFlagOnAnUnknownFieldIsADiagnostic()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch("""{"kind":"set_hidden","path":"nope","hidden":true}"""));

        Assert.Equal(EffectCodes.PathUnknown, Assert.Single(result.Problems).Code);
        Assert.Empty(form.Presentation.HiddenPaths);
    }

    // ------------------------------------------------------------------------------------------
    // options and labels

    /// <summary>A select's candidates are replaced with labels, in display order.</summary>
    [Fact]
    public void OptionsAreReplacedInOrder()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_options","path":"status","options":[{"value":"draft"},{"value":"open","label":"Open for work"}]}"""));

        Assert.False(result.HasProblems);
        IReadOnlyList<OptionItem> options = form.Presentation.OptionsFor("status")!;
        Assert.Equal(OptionValues, options.Select(option => option.Value));
        Assert.Equal("Open for work", options[1].DisplayLabel);
        Assert.Equal("open", options[1].DisplayLabel == "Open for work" ? "open" : "wrong");
    }

    /// <summary>A duplicate candidate is refused.</summary>
    [Fact]
    public void DuplicateOptionsAreRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_options","path":"customer","options":[{"value":"a"},{"value":"a"}]}"""));

        Assert.Equal(EffectCodes.OptionInvalid, Assert.Single(result.Problems).Code);
        Assert.Null(form.Presentation.OptionsFor("customer"));
    }

    /// <summary>Candidate values for a field that is neither a select nor a link are refused.</summary>
    [Fact]
    public void OptionsOnATextFieldAreRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_options","path":"title","options":[{"value":"a"}]}"""));

        Assert.Equal(EffectCodes.OptionInvalid, Assert.Single(result.Problems).Code);
    }

    /// <summary>More candidates than the cap allows is a limit problem, not an allocation.</summary>
    [Fact]
    public void TooManyOptionsHitTheLimit()
    {
        FormState form = TestForm.Load();
        var applier = new EffectApplier(new EffectLimits(MaxOptions: 2));
        ApplyResult result = applier.Apply(form, TestForm.Batch(
            """{"kind":"set_options","path":"customer","options":[{"value":"a"},{"value":"b"},{"value":"c"}]}"""));

        Assert.Equal(EffectCodes.OptionInvalid, Assert.Single(result.Problems).Code);
    }

    /// <summary>A blank label is refused; a good one replaces the field's label.</summary>
    [Fact]
    public void LabelsAreCheckedThenKept()
    {
        FormState form = TestForm.Load();
        ApplyResult refused = Applier.Apply(form, TestForm.Batch("""{"kind":"set_label","path":"qty","label":"   "}"""));
        Assert.Equal(EffectCodes.LabelInvalid, Assert.Single(refused.Problems).Code);

        ApplyResult kept = Applier.Apply(form, TestForm.Batch("""{"kind":"set_label","path":"qty","label":"Quantity"}"""));
        Assert.False(kept.HasProblems);
        Assert.Equal("Quantity", form.Presentation.LabelFor("qty"));
    }

    // ------------------------------------------------------------------------------------------
    // actions

    /// <summary>An action is added, re-added in place, and removed by id.</summary>
    [Fact]
    public void ActionsReplaceInPlaceAndRemove()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"add_action","id":"pick","label":"Pick","effect":{"kind":"focus","path":"qty"}}""",
            """{"kind":"add_action","id":"other","label":"Other","effect":{"kind":"focus","path":"note"}}""",
            """{"kind":"add_action","id":"pick","label":"Pick again","effect":{"kind":"focus","path":"qty"}}"""));

        Assert.False(result.HasProblems);
        Assert.Equal(ActionsAfterAdd, form.Actions.Select(action => action.Id));
        Assert.Equal("Pick again", form.Actions[0].Label);

        ApplyResult removed = Applier.Apply(form, TestForm.Batch("""{"kind":"remove_action","id":"pick"}"""));
        Assert.False(removed.HasProblems);
        Assert.Equal(ActionsAfterRemove, form.Actions.Select(action => action.Id));
    }

    /// <summary>A malformed action id is refused; nothing is added.</summary>
    [Fact]
    public void AMalformedActionIdIsRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"add_action","id":"Pick Customer","label":"Pick","effect":{"kind":"focus","path":"qty"}}"""));

        Assert.Equal(EffectCodes.ActionInvalid, Assert.Single(result.Problems).Code);
        Assert.Empty(form.Actions);
    }

    /// <summary>A chosen action applies the effect it carries — the one thing the server sent as data.</summary>
    [Fact]
    public void AChosenActionAppliesItsEffect()
    {
        FormState form = TestForm.Load();
        Applier.Apply(form, TestForm.Batch(
            """{"kind":"add_action","id":"fill","label":"Fill","effect":{"kind":"set_value","path":"title","value":"from the action"}}"""));

        ApplyResult result = Applier.ApplyAction(form, form.Actions[0]);

        Assert.False(result.HasProblems);
        Assert.Equal("from the action", TestForm.Text(form, "title"));
    }

    // ------------------------------------------------------------------------------------------
    // messages, dialogs, focus, filters

    /// <summary>Messages carry their level, text, key and field, in order.</summary>
    [Fact]
    public void MessagesAreStateTheRendererShows()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"show_message","level":"info","text":"Heads up"}""",
            """{"kind":"show_message","level":"error","text":"Qty must be even","field":"qty"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal(2, form.Messages.Count);
        Assert.Equal(MessageLevel.Info, form.Messages[0].Level);
        Assert.Equal("qty", form.Messages[1].Field);
    }

    /// <summary>A message with neither text nor a key, a key that is not dotted lower-case, and
    /// a message about an unknown field are all refused.</summary>
    [Fact]
    public void MalformedMessagesAreRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, EffectBatch.Of(
            new ShowMessageEffect(MessageLevel.Info, null, null, null),
            new ShowMessageEffect(MessageLevel.Info, "hi", "Not A Key", null),
            new ShowMessageEffect(MessageLevel.Info, "hi", null, "nope"),
            new ShowMessageEffect(MessageLevel.Info, new string('x', 5000), null, null)));

        Assert.Equal(4, result.Problems.Count);
        Assert.Equal(EffectCodes.MessageInvalid, result.Problems[0].Code);
        Assert.Equal(EffectCodes.MessageInvalid, result.Problems[1].Code);
        // A message about a field the form does not have is an unknown path, as server-side
        // validation reports it.
        Assert.Equal(EffectCodes.PathUnknown, result.Problems[2].Code);
        Assert.Equal("nope", result.Problems[2].Path);
        Assert.Equal(EffectCodes.MessageInvalid, result.Problems[3].Code);
        Assert.Empty(form.Messages);
    }

    /// <summary>A dialog request is an intent, with its parameters kept as data.</summary>
    [Fact]
    public void ADialogRequestIsAnIntent()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"open_dialog","dialog_id":"pick_customer","params":{"search":"acme"}}"""));

        Assert.False(result.HasProblems);
        DialogIntent dialog = Assert.Single(form.Dialogs);
        Assert.Equal("pick_customer", dialog.DialogId);
        Assert.Equal("""{"search":"acme"}""", dialog.ParamsJson);
    }

    /// <summary>A head that knows its dialogs refuses one it does not register.</summary>
    [Fact]
    public void AnUnregisteredDialogIsRefusedWhenTheHeadKnowsItsDialogs()
    {
        FormState form = TestForm.Load();
        var applier = new EffectApplier(registeredDialogs: new HashSet<string>(StringComparer.Ordinal) { "pick_customer" });
        ApplyResult result = applier.Apply(form, TestForm.Batch("""{"kind":"open_dialog","dialog_id":"nope"}"""));

        Assert.Equal(EffectCodes.DialogUnknown, Assert.Single(result.Problems).Code);
        Assert.Empty(form.Dialogs);
    }

    /// <summary>Focus moves, and the last focus of a batch wins.</summary>
    [Fact]
    public void FocusMovesToTheLastNamedField()
    {
        FormState form = TestForm.Load(withRow: true);
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"focus","path":"qty"}""",
            """{"kind":"focus","path":"items[1].qty"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("items[1].qty", form.Focus);
    }

    /// <summary>A link filter is kept when the shared expression language parses it.</summary>
    [Fact]
    public void AFilterIsKeptWhenItParses()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_filter","link_path":"customer","filter":"doc.status == 'open'"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("doc.status == 'open'", form.Presentation.FilterFor("customer"));
    }

    /// <summary>A filter that does not parse, and a filter on a field that is not a link, are refused.</summary>
    [Fact]
    public void MalformedFiltersAreRefused()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_filter","link_path":"customer","filter":"doc.status =="}""",
            """{"kind":"set_filter","link_path":"title","filter":"doc.status == 'open'"}"""));

        Assert.Equal(2, result.Problems.Count);
        Assert.All(result.Problems, problem => Assert.Equal(EffectCodes.FilterInvalid, problem.Code));
        Assert.Empty(form.Presentation.Filters);
    }

    // ------------------------------------------------------------------------------------------
    // ordering, batching, limits and the unknown

    /// <summary>Effects apply in order, so a later write of the same field wins.</summary>
    [Fact]
    public void EffectsApplyInOrderAndTheLastWriteWins()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_value","path":"title","value":"one"}""",
            """{"kind":"set_label","path":"note","label":"Note"}""",
            """{"kind":"set_value","path":"title","value":"two"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("two", TestForm.Text(form, "title"));
        Assert.Equal(KindsOfThree, result.AppliedKinds);
    }

    /// <summary>A nested batch is flattened in place: nesting never changes the order.</summary>
    [Fact]
    public void ANestedBatchIsFlattenedInPlace()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, TestForm.Batch(
            """{"kind":"set_label","path":"qty","label":"Before"}""",
            """
            {"kind":"batch","effects":[
              {"kind":"set_label","path":"qty","label":"Inside"},
              {"kind":"batch","effects":[{"kind":"set_value","path":"qty","value":9}]}
            ]}
            """,
            """{"kind":"set_label","path":"note","label":"After"}"""));

        Assert.False(result.HasProblems);
        Assert.Equal("Inside", form.Presentation.LabelFor("qty"));
        Assert.Equal("9", TestForm.Text(form, "qty"));
        Assert.Equal(KindsNested, result.AppliedKinds);
    }

    /// <summary>A batch nested deeper than the cap is refused with a limit problem.</summary>
    [Fact]
    public void ABatchNestedTooDeeplyHitsTheLimit()
    {
        FormState form = TestForm.Load();
        Effect nested = new FocusEffect("qty");
        for (int i = 0; i <= EffectsProtocol.MaxBatchDepth + 1; i++)
        {
            nested = new BatchEffect(Array.AsReadOnly(new[] { nested }));
        }

        ApplyResult result = Applier.Apply(form, EffectBatch.Of(nested));

        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.LimitExceeded);
        Assert.Null(form.Focus);
    }

    /// <summary>More effects than the cap allows is a limit problem, and the first ones apply.</summary>
    [Fact]
    public void TooManyEffectsHitTheLimit()
    {
        FormState form = TestForm.Load();
        var applier = new EffectApplier(new EffectLimits(MaxEffects: 2));
        var effects = new Effect[]
        {
            new SetLabelEffect("qty", "One"),
            new SetLabelEffect("note", "Two"),
            new SetLabelEffect("title", "Three"),
        };

        ApplyResult result = applier.Apply(form, EffectBatch.Of(effects));

        Assert.Contains(result.Problems, problem => problem.Code == EffectCodes.LimitExceeded);
        Assert.Equal(KindsOfTwo, result.AppliedKinds);
        Assert.Equal("One", form.Presentation.LabelFor("qty"));
        Assert.Null(form.Presentation.LabelFor("title"));
    }

    /// <summary>A kind this client does not know is skipped with a diagnostic, never thrown.</summary>
    [Fact]
    public void AnUnknownKindIsADiagnostic()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, EffectBatch.Of(new UnknownEffect()));

        Assert.Equal(EffectCodes.EffectUnknown, Assert.Single(result.Problems).Code);
        Assert.Empty(result.ChangedPaths);
        Assert.True(result.Applied);
    }

    /// <summary>A batch of a version this client does not speak is refused whole.</summary>
    [Fact]
    public void AnUnknownVersionRefusesTheBatchWhole()
    {
        FormState form = TestForm.Load();
        var batch = new EffectBatch(2, Array.AsReadOnly(new Effect[] { new SetValueEffect("qty", "5") }));

        ApplyResult result = Applier.Apply(form, batch);

        Assert.False(result.Applied);
        Assert.Equal(EffectCodes.ProtocolVersion, Assert.Single(result.Problems).Code);
        Assert.Equal("2", TestForm.Text(form, "qty"));
    }

    /// <summary>A write clears the errors a previous validation left on the field.</summary>
    [Fact]
    public void AWriteClearsTheFieldsStaleErrors()
    {
        FormState form = TestForm.Load();
        form.Draft.Fields["qty"].SetErrors(new[] { new FieldError("qty", "order.qty_odd", "Qty must be even.") });

        Applier.Apply(form, TestForm.Batch("""{"kind":"set_value","path":"qty","value":4}"""));

        Assert.Empty(form.Draft.Fields["qty"].Errors);
    }

    /// <summary>An empty batch changes nothing and reports nothing.</summary>
    [Fact]
    public void AnEmptyBatchChangesNothing()
    {
        FormState form = TestForm.Load();
        ApplyResult result = Applier.Apply(form, EffectBatch.Empty);

        Assert.True(result.Applied);
        Assert.False(result.HasProblems);
        Assert.False(result.ChangedAnything);
        Assert.False(form.Draft.IsDirty);
    }

    private sealed record UnknownEffect : Effect
    {
        public override string Kind => "sparkle";
    }
}
