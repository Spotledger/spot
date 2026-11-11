using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>
/// One form per document, and one document per form: walking back to a row reuses the form it already
/// built, a document moved to its own tab is never built a second time here, and the tab keeps the list.
/// </summary>
public sealed class TabDedupTests
{
    [AvaloniaFact]
    public async Task Walking_back_to_a_row_reuses_its_form()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        var first = rig.Split.Form!.Model;

        await rig.Split.WalkAsync(1);
        await rig.Split.WalkAsync(-1);

        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.Same(first, rig.Split.Form!.Model);
        Assert.Equal(2, rig.Documents.Created);
    }

    [AvaloniaFact]
    public async Task Ctrl_Enter_moves_the_form_to_its_own_tab_and_keeps_the_list()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");

        rig.Press(Key.Enter, KeyModifiers.Control);
        TestHost.Pump();

        Assert.Equal([("task", "T-1")], rig.Shell.Opened);
        Assert.True(rig.Documents.IsInTab("T-1"));
        Assert.Empty(rig.Documents.OpenIds);
        Assert.Null(rig.Split.Form);
        Assert.True(rig.List.Model.Phase == Lists.ListPhase.Ready, "the list stays usable");
        Assert.Equal("split.list", rig.Trace.Current);
    }

    [AvaloniaFact]
    public async Task A_document_open_in_a_tab_is_not_built_a_second_time()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.Press(Key.Enter, KeyModifiers.Control);
        TestHost.Pump();
        var created = rig.Documents.Created;

        // Selecting the row again must not build another form for the document that owns a tab.
        await rig.Split.ShowAsync("T-1");
        TestHost.Pump();

        Assert.Equal(created, rig.Documents.Created);
        Assert.True(rig.Documents.IsInTab("T-1"));
        Assert.Contains("own tab", rig.Trace.LastAnnouncement ?? string.Empty, StringComparison.OrdinalIgnoreCase);
    }

    [AvaloniaFact]
    public async Task Closing_the_tab_lets_the_arrangement_show_the_document_again()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.Press(Key.Enter, KeyModifiers.Control);
        TestHost.Pump();

        Assert.True(rig.Documents.ReleaseTab("T-1"));
        await rig.OpenAsync("T-1");

        Assert.NotNull(rig.Split.Form);
        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
    }

    [AvaloniaFact]
    public async Task A_new_document_is_saved_and_keeps_its_form_under_its_new_id()
    {
        using var rig = new SplitRig(rows: 2);
        rig.Split.FocusListPane();
        rig.Press(Key.N, KeyModifiers.Control | KeyModifiers.Alt);
        SplitRig.PumpUntil(() => rig.Split.Form is { Model.Phase: Spotledger.Client.Forms.FormPhase.Ready }, "the new form");

        rig.TypeInto(" A new task");
        Assert.True(await rig.Split.SaveAsync(), "the new document must save");

        Assert.Equal("T-900", rig.Split.CurrentDocumentId);
        Assert.Single(rig.Documents.OpenIds);
        Assert.Equal("T-900", rig.Documents.OpenIds[0]);
    }
}
