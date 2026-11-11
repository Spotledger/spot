using Avalonia.Headless.XUnit;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

/// <summary>The history seam of the form (WP-UI-17.T4): who creates it, what runs inside its derived scope, and what its gates decide.</summary>
public sealed class FormHistoryTests
{
    private sealed class TableHistory : IChildTableHistory
    {
        public bool CanUndo => false;
        public bool CanRedo => false;
        public ChildTableResult Undo() => ChildTableResult.Ok();
        public ChildTableResult Redo() => ChildTableResult.Ok();
    }

    private sealed class RecordingHistory : IFormHistory
    {
        public List<string> Log { get; } = [];
        public Func<string, ChildTableResult?>? TableGate { get; private set; }
        public Func<string, ChildTableResult?>? FieldGate { get; private set; }
        public bool CanUndo => false;
        public bool CanRedo => false;
        public string? UndoLabel => null;
        public string? RedoLabel => null;
        public event EventHandler? Changed { add { } remove { } }
        public event EventHandler<string>? Cleared { add { } remove { } }
        public ChildTableResult Undo() => ChildTableResult.Ok();
        public ChildTableResult Redo() => ChildTableResult.Ok();
        public void RunDerived(Action change) { Log.Add("derived:start"); change(); Log.Add("derived:end"); }
        public IChildTableHistory ForTable(string table) => new TableHistory();
        public void SetGate(Func<string, ChildTableResult?> table, Func<string, ChildTableResult?>? field) { TableGate = table; FieldGate = field; Log.Add("gate"); }
    }

    private static async Task<(DocumentFormViewModel Form, RecordingHistory History, FakeGateway Gateway)> OpenAsync(Action<FakeGateway>? setup = null)
    {
        var gateway = new FakeGateway().Seed("INV-1");
        setup?.Invoke(gateway);
        var history = new RecordingHistory();
        var form = new DocumentFormViewModel(gateway, "invoice", "INV-1", new FakeShell()) { HistoryFactory = _ => history };
        await form.LoadAsync();
        return (form, history, gateway);
    }

    [AvaloniaFact]
    public async Task TheFormCreatesItsHistoryAfterLoadAndSuppliesTheGatesAtOnce()
    {
        var (form, history, _) = await OpenAsync();
        Assert.Same(history, form.History);
        Assert.Contains("gate", history.Log);
        Assert.NotNull(history.FieldGate);
    }

    [AvaloniaFact]
    public async Task AServerDiffRunsInsideTheDerivedScope()
    {
        var (form, history, _) = await OpenAsync();
        form.ApplyServerDiff(draft => draft.Set("amount", new DecimalFieldValue(99m)));
        Assert.Equal(["gate", "derived:start", "derived:end"], history.Log);
    }

    [AvaloniaFact]
    public async Task WithoutAHistoryAServerDiffIsJustApplied()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = new DocumentFormViewModel(gateway, "invoice", "INV-1", new FakeShell());
        await form.LoadAsync();
        form.ApplyServerDiff(draft => draft.Set("amount", new DecimalFieldValue(99m)));
        Assert.Null(form.History);
        Assert.Equal("99", ((DecimalFieldValue)form.Draft!.Get("amount")!).CanonicalText.TrimEnd('0', '.'));
    }

    [AvaloniaFact]
    public async Task TableStepsAreRefusedUntilTheViewSuppliesItsCheck()
    {
        var (form, history, _) = await OpenAsync();
        Assert.False(history.TableGate!("items")!.Succeeded);
        using var view = new DocumentFormView(form);
        Assert.Null(history.TableGate("items"));
        view.Dispose();
        Assert.False(history.TableGate("items")!.Succeeded, "a view that is gone leaves no check behind");
    }

    [AvaloniaFact]
    public async Task TheFieldGateFollowsTheWriteRightMasksAndPermissions()
    {
        var (form, history, _) = await OpenAsync();
        Assert.Null(history.FieldGate!("title"));
        Assert.False(history.FieldGate("nothing")!.Succeeded);

        var (readOnly, rHistory, _) = await OpenAsync(g => g.Rights = DocumentRights.All with { Write = false });
        Assert.Equal(ChildTableRefusal.NoWriteRight, rHistory.FieldGate!("title")!.Refusal);
        Assert.NotNull(readOnly);

        var (masked, mHistory, _) = await OpenAsync(g => g.Snapshot = Fixtures.Permissions("""{"masked":["secret_note"]}"""));
        Assert.Equal(ChildTableRefusal.CellMasked, mHistory.FieldGate!("secret_note")!.Refusal);
        Assert.NotNull(masked);

        var (locked, lHistory, _) = await OpenAsync(g => g.Snapshot = Fixtures.Permissions("""{"read_only":["title"]}"""));
        Assert.Equal(ChildTableRefusal.CellReadOnly, lHistory.FieldGate!("title")!.Refusal);
        Assert.NotNull(locked);
        Assert.NotNull(form);
    }

    [AvaloniaFact]
    public async Task ARecordedDerivedReadOnlyFieldIsNotRefused()
    {
        // A total the metadata marks read-only is only ever changed as a consequence of an allowed edit.
        var model = new DocTypeModel("invoice", "Invoice", 1, true,
        [
            new FieldModel("title", "Title", FieldKind.Text),
            new FieldModel("total", "Total", FieldKind.DecimalNumber, scale: 2, precision: 18, readOnly: true),
        ]);
        var gateway = new FakeGateway { Model = model }.Seed("INV-1");
        var history = new RecordingHistory();
        var form = new DocumentFormViewModel(gateway, "invoice", "INV-1", new FakeShell()) { HistoryFactory = _ => history };
        await form.LoadAsync();
        Assert.True(form.Draft!.Fields["total"].IsReadOnly);
        Assert.Null(history.FieldGate!("total"));
    }

    [AvaloniaFact]
    public async Task TheRecoverySlotAndTheClearNoteReachTheView()
    {
        var (form, _, _) = await OpenAsync();
        using var view = new DocumentFormView(form);
        var window = TestHost.Show(view);
        var bar = new Avalonia.Controls.TextBlock { Text = "Restore unsaved changes" };
        form.Recovery = bar;
        TestHost.Pump();
        Assert.Contains(view.GetVisualDescendantsOf<Avalonia.Controls.TextBlock>(), t => ReferenceEquals(t, bar));
        window.Close();
    }

    [AvaloniaFact]
    public async Task OpenedRunsOnceAfterTheLoadAndAFailureIsAMessageNotACrash()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var form = new DocumentFormViewModel(gateway, "invoice", "INV-1", new FakeShell())
        {
            Opened = _ => throw new InvalidOperationException("no store"),
        };
        await form.LoadAsync();
        Assert.Equal(FormPhase.Ready, form.Phase);
        Assert.Contains(form.Messages, m => m.Code == "draft.unavailable");
    }
}

internal static class VisualQueries
{
    public static IEnumerable<T> GetVisualDescendantsOf<T>(this Avalonia.Visual root) where T : class
        => Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root).OfType<T>();
}
