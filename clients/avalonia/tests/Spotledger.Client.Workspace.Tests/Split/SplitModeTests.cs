using Avalonia;
using Avalonia.Automation;
using Avalonia.Input;
using Avalonia.Media;
using SplitView = Spotledger.Client.Workspace.Split.ListFormSplit;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Workspace.Split;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>
/// The split-mode matrix: three modes, in a left-to-right and in a right-to-left layout, the mode
/// remembered per DocType and per person, and the splitter resized from the keyboard.
/// </summary>
public sealed class SplitModeTests
{
    [AvaloniaTheory]
    [InlineData(SplitMode.List)]
    [InlineData(SplitMode.Split)]
    [InlineData(SplitMode.Form)]
    public void A_mode_shows_exactly_the_panes_it_names(SplitMode mode)
    {
        foreach (var flow in new[] { FlowDirection.LeftToRight, FlowDirection.RightToLeft })
        {
            using var rig = new SplitRig(rows: 3, flow: flow);
            rig.Split.SetMode(mode);
            TestHost.Pump();

            var list = SplitRig.Find<Avalonia.Controls.Border>(rig.Split, SplitView.ListPaneId);
            var form = SplitRig.Find<Avalonia.Controls.Border>(rig.Split, SplitView.FormPaneId);
            Assert.NotNull(list);
            Assert.NotNull(form);
            Assert.Equal(mode != SplitMode.Form, list.IsVisible);
            Assert.Equal(mode != SplitMode.List, form.IsVisible);

            // The splitter is only a thing to grab in the split mode.
            var splitter = SplitRig.Find<Avalonia.Controls.GridSplitter>(rig.Split, SplitView.SplitterId);
            Assert.NotNull(splitter);
            Assert.Equal(mode == SplitMode.Split, splitter.IsVisible);
        }
    }

    [AvaloniaFact]
    public void The_mode_is_remembered_per_doctype_and_per_person()
    {
        using var rig = new SplitRig(rows: 2);
        rig.Split.SetMode(SplitMode.Form);

        Assert.Equal(SplitMode.Form, rig.Preferences.Load(SplitRig.User, "task")!.Mode);
        Assert.Null(rig.Preferences.Load("bob", "task"));
    }

    [AvaloniaFact]
    public void The_splitter_resizes_from_the_keyboard_and_remembers_the_share()
    {
        using var rig = new SplitRig(rows: 2);
        rig.Split.SetMode(SplitMode.Split);
        TestHost.Pump();

        var splitter = SplitRig.Find<Avalonia.Controls.GridSplitter>(rig.Split, SplitView.SplitterId)!;
        Assert.True(splitter.Focus());
        TestHost.Pump();

        var before = rig.Split.ListPercent;
        rig.Press(Key.Right);
        Assert.True(rig.Split.ListPercent > before, "Right on the splitter gives the list more room");

        var wider = rig.Split.ListPercent;
        rig.Press(Key.Left, KeyModifiers.Shift);
        Assert.Equal(Math.Max(SplitPreference.MinListPercent, wider - 10), rig.Split.ListPercent);

        var saved = rig.Preferences.Load(SplitRig.User, "task");
        Assert.NotNull(saved);
        Assert.Equal(rig.Split.ListPercent, saved.ListPercent);
    }

    [AvaloniaFact]
    public void The_share_stops_at_its_bounds()
    {
        using var rig = new SplitRig(rows: 2);
        rig.Split.SetMode(SplitMode.Split);

        for (var i = 0; i < 20; i++)
        {
            rig.Split.Nudge(10);
        }

        Assert.Equal(SplitPreference.MaxListPercent, rig.Split.ListPercent);
        for (var i = 0; i < 20; i++)
        {
            rig.Split.Nudge(-10);
        }

        Assert.Equal(SplitPreference.MinListPercent, rig.Split.ListPercent);
    }

    [AvaloniaFact]
    public void The_mode_chords_switch_the_arrangement()
    {
        using var rig = new SplitRig(rows: 2);
        rig.Split.FocusListPane();
        TestHost.Pump();

        rig.Press(Key.D1, KeyModifiers.Control | KeyModifiers.Alt);
        Assert.Equal(SplitMode.List, rig.Split.Mode);

        rig.Press(Key.D2, KeyModifiers.Control | KeyModifiers.Alt);
        Assert.Equal(SplitMode.Split, rig.Split.Mode);

        rig.Press(Key.D3, KeyModifiers.Control | KeyModifiers.Alt);
        Assert.Equal(SplitMode.Form, rig.Split.Mode);

        // The mode text says where the arrangement is, for a person and for a screen reader.
        var text = SplitRig.Find<Avalonia.Controls.TextBlock>(rig.Split, "split.mode");
        Assert.NotNull(text);
        Assert.Equal("Form only", text.Text);
    }

    [AvaloniaFact]
    public void The_panes_carry_automation_ids_a_screen_reader_can_name()
    {
        using var rig = new SplitRig(rows: 2);
        var list = SplitRig.Find<Avalonia.Controls.Border>(rig.Split, SplitView.ListPaneId);
        var form = SplitRig.Find<Avalonia.Controls.Border>(rig.Split, SplitView.FormPaneId);
        Assert.Equal("split.list", AutomationProperties.GetAutomationId(list!));
        Assert.Equal("split.form", AutomationProperties.GetAutomationId(form!));
        Assert.False(string.IsNullOrEmpty(AutomationProperties.GetName(list!)));
    }
}
