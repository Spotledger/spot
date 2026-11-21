using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Workspace.Split;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>
/// Walking a worklist from inside the form: Alt+Down and Alt+Up select the next and the previous row
/// without leaving the form, through a filtered list, at the first and the last row, and on an empty
/// list. The keys reach the list from the form's own field, which is where a person actually is.
/// </summary>
public sealed class WalkListTests
{
    [AvaloniaFact]
    public async Task Alt_down_and_up_walk_the_list_and_keep_the_focus_in_the_form()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        Assert.Equal("field:title", rig.Trace.Current);

        rig.Press(Key.Down, KeyModifiers.Alt);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId == "T-2", "Alt+Down to select the second row");
        Assert.Equal("field:title", rig.Trace.Current);

        rig.Press(Key.Up, KeyModifiers.Alt);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId == "T-1", "Alt+Up to go back");

        // The list's selection follows, so the row a person sees is the row the form shows.
        Assert.Equal(["T-1"], rig.List.Model.SelectedIds);
    }

    [AvaloniaFact]
    public async Task The_last_row_says_so_and_the_selection_stays_put()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-3");
        Assert.Equal("T-3", rig.Split.CurrentDocumentId);

        rig.Press(Key.Down, KeyModifiers.Alt);
        Assert.Equal("T-3", rig.Split.CurrentDocumentId);
        Assert.Contains("last row", rig.Trace.LastAnnouncement, StringComparison.OrdinalIgnoreCase);
    }

    [AvaloniaFact]
    public async Task The_first_row_says_so()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");

        rig.Press(Key.Up, KeyModifiers.Alt);
        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.Contains("first row", rig.Trace.LastAnnouncement, StringComparison.OrdinalIgnoreCase);
    }

    [AvaloniaFact]
    public async Task An_empty_list_announces_it_and_keeps_the_form()
    {
        using var rig = new SplitRig(rows: 0);
        Assert.Equal(0, rig.List.Grid.RowCount);
        rig.Split.FocusListPane();
        TestHost.Pump();

        rig.Press(Key.Down, KeyModifiers.Alt);
        Assert.Contains("no rows", rig.Trace.LastAnnouncement, StringComparison.OrdinalIgnoreCase);
        Assert.Null(rig.Split.CurrentDocumentId);
    }

    [AvaloniaFact]
    public async Task A_filtered_list_walks_its_own_rows()
    {
        using var rig = new SplitRig(rows: 15);
        // Filter with the keyboard: the quick-search box narrows the list; no pointer is used.
        await rig.List.Model.SetSearchTextAsync("Task 1");
        SplitRig.PumpUntil(() => rig.List.Model.Phase == Lists.ListPhase.Ready && rig.List.Grid.RowCount < 15, "the filtered list");

        var rows = rig.List.Grid.RowCount;
        Assert.True(rows > 1, "the filter must leave more than one row for walking to be interesting");

        await rig.OpenAsync("T-1");

        rig.Press(Key.Down, KeyModifiers.Alt);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId != "T-1", "the walk to move");
        Assert.NotEqual("T-1", rig.Split.CurrentDocumentId);
        Assert.Equal(1, rig.List.Grid.SelectedRows[0]);
    }

    [AvaloniaFact]
    public async Task A_walk_is_blocked_while_the_guard_waits_for_an_answer()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();
        Assert.True(rig.Split.IsGuardShown);

        rig.Press(Key.Down, KeyModifiers.Alt);
        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.Contains("unsaved-changes", rig.Trace.LastAnnouncement, StringComparison.OrdinalIgnoreCase);
    }
}
