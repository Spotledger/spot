using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Split;

/// <summary>
/// Ctrl+Z and Ctrl+Y outside a text editor route to the form's history (the workspace undo stack behind
/// the host's adapter), so a keyboard-only person can undo without the Edit menu. Inside a text editor
/// the field keeps both keys, which is why they are not workspace chords.
/// <para>The draft is changed through <see cref="DocumentDraft.Set"/> here, which is what notifies the
/// history. An edit typed into a field control commits to the field's state without raising
/// <see cref="DocumentDraft.FieldChanged"/>, so the stack does not record it: that seam belongs to
/// Forms/Controls and is written up in the report's open questions.</para>
/// </summary>
public sealed class UndoRoutingTests
{
    [AvaloniaFact]
    public async Task Ctrl_z_and_ctrl_y_reach_the_forms_history_from_the_keyboard()
    {
        using var rig = new SplitRig(rows: 2);
        await rig.OpenAsync("T-1");
        var model = rig.Split.Form!.Model;

        // Escape leaves the text editor first (its LostFocus commit would otherwise write the old
        // text back over the change), so Ctrl+Z is the form's history rather than the field's.
        rig.Press(Key.Escape);
        model.Draft!.Set("title", new TextFieldValue("Changed"));
        Assert.True(model.History!.CanUndo, "the history must have a step to undo");

        // A bare Z or Y is only a letter: it must never undo or redo.
        rig.Press(Key.Z);
        rig.Press(Key.Y);
        Assert.Equal("Changed", ((TextFieldValue)model.Draft.Get("title")!).Value);
        Assert.False(model.History.CanRedo);

        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal("Task 1", ((TextFieldValue)model.Draft.Get("title")!).Value);
        Assert.True(model.History.CanRedo);

        rig.Press(Key.Y, KeyModifiers.Control);
        Assert.Equal("Changed", ((TextFieldValue)model.Draft.Get("title")!).Value);
    }

    [AvaloniaFact]
    public async Task Ctrl_z_inside_a_text_editor_is_the_editors_own_and_is_left_alone()
    {
        using var rig = new SplitRig(rows: 2);
        await rig.OpenAsync("T-1");
        var model = rig.Split.Form!.Model;
        model.Draft!.Set("title", new TextFieldValue("Changed"));
        var edits = 0;
        model.Draft.FieldChanged += (_, _) => edits++;

        // Focus is on the title editor: the split must not consume the key.
        rig.Press(Key.Z, KeyModifiers.Control);
        Assert.Equal("Changed", ((TextFieldValue)model.Draft.Get("title")!).Value);
        Assert.Equal(0, edits);
    }
}
