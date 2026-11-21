using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Metadata;
using Spotledger.Client.Workspace.Split;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>
/// The unsaved-changes guard: a selection change never closes a dirty form silently. The change waits,
/// the guard says what is at stake, and one answer decides — save and move on, discard and move on, or
/// stay where you are.
/// </summary>
public sealed class SelectionGuardTests
{
    [AvaloniaFact]
    public async Task A_dirty_form_holds_the_selection_change_and_shows_the_guard()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");

        Assert.True(rig.Split.Form!.Model.IsDirty, "typing must make the form dirty before the guard is tested");
        Assert.False(rig.Split.IsGuardShown);

        // The list's own selection event is the trigger, exactly as a click or an arrow key would raise it.
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();

        Assert.True(rig.Split.IsGuardShown);
        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.True(rig.Split.Guard.IsPending);
        Assert.Equal("T-2", rig.Split.Guard.PendingId);
        Assert.Contains("unsaved changes", rig.Split.GuardQuestion.Text, StringComparison.OrdinalIgnoreCase);
    }

    [AvaloniaFact]
    public async Task Stay_here_keeps_the_document_the_changes_and_the_selection()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();
        Assert.True(rig.Split.IsGuardShown);

        await rig.Split.AnswerGuardAsync(GuardChoice.Cancel);
        TestHost.Pump();

        Assert.False(rig.Split.IsGuardShown);
        Assert.False(rig.Split.Guard.IsPending);
        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.True(rig.Split.Form!.Model.IsDirty, "the changes survive a cancelled selection");
        Assert.Equal(["T-1"], rig.List.Model.SelectedIds);
    }

    [AvaloniaFact]
    public async Task Discard_throws_the_changes_away_and_shows_the_row_that_was_waiting()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");
        var dirty = rig.Split.Form!.Model;
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();

        await rig.Split.AnswerGuardAsync(GuardChoice.Discard);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId == "T-2", "T-2 to open");

        Assert.False(rig.Split.IsGuardShown);
        Assert.Equal("T-2", rig.Split.CurrentDocumentId);
        Assert.NotSame(dirty, rig.Split.Form!.Model);
        Assert.False(rig.Split.Form.Model.IsDirty);
        // The discarded document was never sent.
        Assert.Empty(rig.DocumentGateway.Patches);
    }

    [AvaloniaFact]
    public async Task Save_sends_the_changes_and_then_shows_the_row_that_was_waiting()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();

        await rig.Split.AnswerGuardAsync(GuardChoice.Save);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId == "T-2", "T-2 to open");

        Assert.False(rig.Split.IsGuardShown);
        Assert.Equal("T-2", rig.Split.CurrentDocumentId);
        var patch = Assert.Single(rig.DocumentGateway.Patches);
        Assert.Contains("edited", patch, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public async Task A_selection_change_on_a_clean_form_is_not_guarded()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.List.Grid.SetSelection([2]);
        SplitRig.PumpUntil(() => rig.Split.CurrentDocumentId == "T-3", "T-3 to open");

        Assert.False(rig.Split.IsGuardShown);
        Assert.False(rig.Split.Guard.IsPending);
    }

    [AvaloniaFact]
    public async Task Escape_answers_the_guard_with_stay_here()
    {
        using var rig = new SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.TypeInto(" edited");
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();
        Assert.True(rig.Split.IsGuardShown);

        rig.Press(Key.Escape);
        SplitRig.PumpUntil(() => !rig.Split.Guard.IsPending, "the guard to be answered by Esc");

        Assert.Equal("T-1", rig.Split.CurrentDocumentId);
        Assert.True(rig.Split.Form!.Model.IsDirty);
    }
}
