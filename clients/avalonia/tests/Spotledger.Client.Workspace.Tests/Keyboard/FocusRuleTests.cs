using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Api;
using Spotledger.Client.Workspace.Keyboard;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Keyboard;

/// <summary>
/// The focus rules, one test each: where focus goes after open, save, delete, cancel, an error, a dialog
/// close, a pane switch, a tab switch, a refresh, and a validation failure — plus the two invariants the
/// whole arrangement rests on, that focus never lands on a disabled or hidden control and that a
/// validation failure both moves the focus and says why.
/// </summary>
public sealed class FocusRuleTests
{
    [AvaloniaFact]
    public void After_open_focus_goes_to_the_form_and_its_first_field()
    {
        var target = FocusRules.Resolve(FocusRule.AfterOpen, new FocusContext { Previous = FocusRules.List });
        Assert.Equal(FocusRules.Form, target.AutomationId);
        Assert.Null(target.Announcement);
    }

    [AvaloniaFact]
    public void After_save_focus_stays_where_the_person_left_it()
    {
        var target = FocusRules.Resolve(FocusRule.AfterSave, new FocusContext { Previous = FocusRules.Field("title") });
        Assert.Equal(FocusRules.Field("title"), target.AutomationId);
        Assert.Equal("Saved", target.Announcement);
    }

    [AvaloniaFact]
    public void After_delete_focus_goes_back_to_the_list()
    {
        var target = FocusRules.Resolve(FocusRule.AfterDelete, new FocusContext { Previous = FocusRules.Form });
        Assert.Equal(FocusRules.List, target.AutomationId);
        Assert.False(string.IsNullOrEmpty(target.Announcement));
    }

    [AvaloniaFact]
    public void After_cancel_focus_goes_back_to_the_list()
    {
        var target = FocusRules.Resolve(FocusRule.AfterCancel, new FocusContext { Previous = FocusRules.Form });
        Assert.Equal(FocusRules.List, target.AutomationId);
    }

    [AvaloniaFact]
    public void After_an_error_focus_goes_to_the_named_field_and_the_message_is_announced()
    {
        var target = FocusRules.Resolve(FocusRule.AfterError, new FocusContext { InvalidField = "title", Message = "Title is required" });
        Assert.Equal(FocusRules.Field("title"), target.AutomationId);
        Assert.Equal("Title is required", target.Announcement);

        // Without a field the form itself takes the focus, never nothing.
        var withoutField = FocusRules.Resolve(FocusRule.AfterError, new FocusContext { Message = "Save failed" });
        Assert.Equal(FocusRules.Form, withoutField.AutomationId);
    }

    [AvaloniaFact]
    public void After_a_dialog_closes_focus_returns_to_the_control_that_opened_it()
    {
        var target = FocusRules.Resolve(FocusRule.AfterDialogClose, new FocusContext { Previous = FocusRules.Field("done") });
        Assert.Equal(FocusRules.Field("done"), target.AutomationId);
    }

    [AvaloniaFact]
    public void After_a_pane_switch_focus_goes_to_the_pane_that_became_active()
    {
        var target = FocusRules.Resolve(FocusRule.AfterPaneSwitch, new FocusContext { Pane = FocusRules.List });
        Assert.Equal(FocusRules.List, target.AutomationId);
    }

    [AvaloniaFact]
    public void After_a_tab_switch_focus_goes_to_the_new_tabs_list()
    {
        var target = FocusRules.Resolve(FocusRule.AfterTabSwitch, new FocusContext { Pane = FocusRules.List });
        Assert.Equal(FocusRules.List, target.AutomationId);
    }

    [AvaloniaFact]
    public void After_a_refresh_focus_stays_put_and_the_count_is_announced()
    {
        var target = FocusRules.Resolve(FocusRule.AfterRefresh, new FocusContext { Previous = FocusRules.List, Message = "15 documents" });
        Assert.Equal(FocusRules.List, target.AutomationId);
        Assert.Equal("15 documents", target.Announcement);
    }

    [AvaloniaFact]
    public void A_validation_failure_focuses_the_first_invalid_field_and_scrolls_it_into_view()
    {
        var target = FocusRules.Resolve(
            FocusRule.AfterValidationFailure,
            new FocusContext { InvalidField = "title", Message = "Title is required" });

        Assert.Equal(FocusRules.Field("title"), target.AutomationId);
        Assert.True(target.ScrollIntoView);
        Assert.Equal("Title is required", target.Announcement);
    }

    [AvaloniaFact]
    public async Task A_failed_save_moves_the_focus_to_the_invalid_field_and_announces_the_message()
    {
        using var rig = new Split.SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        rig.DocumentGateway.FailNextPatch = new ApiError
        {
            Code = "validation.failed",
            Message = "The document was not saved",
            StatusCode = 422,
            FieldErrors = [new ApiFieldError("title", "required", "Title is required")],
        };
        rig.TypeInto(" edited");

        rig.Press(Key.S, KeyModifiers.Control | KeyModifiers.Alt);
        Split.SplitRig.PumpUntil(
            () => rig.Trace.LastAnnouncement?.Contains("required", StringComparison.OrdinalIgnoreCase) == true,
            "the validation message to be announced");

        Assert.Equal("field:title", rig.Trace.Current);
        Assert.Contains("Title is required", rig.Trace.LastAnnouncement!, StringComparison.Ordinal);
        Assert.Contains(rig.Trace.Entries, entry => entry.Kind == FocusKind.Rule && entry.Note == nameof(FocusRule.AfterValidationFailure));
        Assert.True(rig.Split.Form!.Model.IsDirty, "a refused save keeps the changes");
    }

    [AvaloniaFact]
    public async Task Esc_returns_the_focus_to_the_list_row()
    {
        using var rig = new Split.SplitRig(rows: 3);
        await rig.OpenAsync("T-1");
        Assert.Equal("field:title", rig.Trace.Current);

        rig.Press(Key.Escape);
        Assert.Equal("split.list", rig.Trace.Current);
        Assert.Equal(["T-1"], rig.List.Model.SelectedIds);
    }

    [AvaloniaFact]
    public async Task Focus_never_lands_on_a_disabled_or_hidden_control()
    {
        using var rig = new Split.SplitRig(rows: 3);
        await rig.OpenAsync("T-1");

        // A tour of the panes, the modes and the guard: every focus move the arrangement makes.
        rig.Press(Key.F6);
        rig.Press(Key.F6);
        rig.Press(Key.F6, KeyModifiers.Shift);
        rig.Press(Key.D1, KeyModifiers.Control | KeyModifiers.Alt);
        rig.Press(Key.D3, KeyModifiers.Control | KeyModifiers.Alt);
        rig.Press(Key.D2, KeyModifiers.Control | KeyModifiers.Alt);
        rig.TypeInto(" edited");
        rig.List.Grid.SetSelection([1]);
        TestHost.Pump();
        rig.Press(Key.Escape);

        Assert.Empty(rig.Trace.Violations);
        Assert.NotNull(rig.Trace.Current);
    }

    [AvaloniaFact]
    public void A_form_that_does_not_load_says_so_and_keeps_the_focus_in_the_form()
    {
        using var rig = new Split.SplitRig(rows: 1);
        rig.ListGateway.Rows.Clear();
        var failed = rig.Split.ShowAsync("T-404");
        Split.SplitRig.PumpUntil(() => failed.IsCompleted, "the failed load");
        TestHost.Pump();

        Assert.Equal("split.form", rig.Trace.Current);
        Assert.Contains("No such document", rig.Trace.LastAnnouncement ?? string.Empty, StringComparison.Ordinal);
    }
}
