using Spotledger.Client.Designer;
using Xunit;

namespace Spotledger.Designer.Tests;

/// <summary>
/// The keyboard route: every canvas action a pointer can perform has a key gesture, an insert by
/// key goes through the same field dialog as a drop, and a design can be laid out without a mouse.
/// </summary>
public sealed class KeyboardCanvasTests
{
    [Fact]
    public void EveryCanvasActionHasAGestureThatResolvesBackToIt()
    {
        var kinds = Enum.GetValues<CanvasGesture>();
        foreach (var kind in kinds)
        {
            var gesture = CanvasKeyboardMap.GestureText(kind);
            Assert.False(string.IsNullOrWhiteSpace(gesture));
            Assert.Equal(kind, CanvasKeyboardMap.Resolve(gesture));
        }

        // The view builds the name from a key event, so the arrow names and a lower-case chord
        // have to resolve too.
        Assert.Equal(CanvasGesture.MoveUp, CanvasKeyboardMap.Resolve("alt+ArrowUp"));
        Assert.Equal(CanvasGesture.NextSection, CanvasKeyboardMap.Resolve("Alt+Shift+ArrowDown"));
        Assert.Equal(CanvasGesture.Insert, CanvasKeyboardMap.Resolve("control+i"));
        Assert.Null(CanvasKeyboardMap.Resolve("Ctrl+Q"));
        Assert.Null(CanvasKeyboardMap.Resolve(""));
    }

    [Fact]
    public async Task ADesignCanBeBuiltAndReorderedWithoutAPointer()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        var designer = harness.Designer;
        await harness.AddSectionAsync("Request", "request", 2);

        // Ctrl+I on an empty section: the same dialog a drop opens, the same two ops.
        designer.SelectedPaletteType = "date";
        var (handled, _) = await designer.HandleGestureAsync("Ctrl+I");
        Assert.True(handled);
        Assert.True(designer.FieldDialog.IsOpen);
        designer.FieldDialog.Label = "Request Date";
        await designer.FieldDialog.CreateAsync();

        // Ctrl+I with a field selected: inserted after it.
        designer.SelectedPaletteType = "text";
        await designer.HandleGestureAsync("Ctrl+I");
        designer.FieldDialog.Label = "Customer Name";
        await designer.FieldDialog.CreateAsync();

        // Alt+Up on the second field cannot pass the first: the layout's reorder op anchors a
        // field after another, and "no anchor" means the end, so the designer says so instead of
        // silently moving the field to the bottom.
        designer.SelectField("customer_name");
        var (upHandled, upResult) = await designer.HandleGestureAsync("Alt+Up");
        Assert.True(upHandled);
        Assert.False(upResult.Ok);

        // Alt+Down on the first field does reorder: it lands after the second.
        designer.SelectField("request_date");
        var (downHandled, downResult) = await designer.HandleGestureAsync("Alt+Down");
        Assert.True(downHandled);
        Assert.True(downResult.Ok);
        var afterDown = new[] { "customer_name", "request_date" };
        Assert.Equal(afterDown, harness.Server.Current.Section("request", null)!.Items.ToArray());

        // Alt+Right and Alt+Left resize the selected section's span.
        designer.SelectSection("request");
        await designer.HandleGestureAsync("Alt+Right");
        Assert.Equal(3, harness.Server.Current.Section("request", null)!.Columns);
        await designer.HandleGestureAsync("Alt+Left");
        Assert.Equal(2, harness.Server.Current.Section("request", null)!.Columns);

        // Alt+Shift+Down moves the selected field into the next section.
        await harness.AddSectionAsync("Details", "details");
        designer.SelectField("customer_name");
        var (acrossHandled, acrossResult) = await designer.HandleGestureAsync("Alt+Shift+Down");
        Assert.True(acrossHandled);
        Assert.True(acrossResult.Ok);
        var moved = new[] { "customer_name" };
        var left = new[] { "request_date" };
        Assert.Equal(moved, harness.Server.Current.Section("details", null)!.Items.ToArray());
        Assert.Equal(left, harness.Server.Current.Section("request", null)!.Items.ToArray());

        var expected = new[]
        {
            "add_form_section|request|Request|2|-|-",
            "add_field|request_date|Request Date|date|-|-",
            "move_field_in_layout|request_date|request|-|-",
            "add_field|customer_name|Customer Name|text|request_date|-",
            "move_field_in_layout|customer_name|request|request_date|-",
            "move_field_in_layout|request_date|request|-|-",
            "set_section_columns|request|3|-",
            "set_section_columns|request|2|-",
            "add_form_section|details|Details|-|-|-",
            "move_field_in_layout|customer_name|details|-|-",
        };
        Assert.Equal(expected, harness.IssuedOps);
    }

    [Fact]
    public async Task KeyboardInsertWithoutAPaletteChoiceIsRefused()
    {
        var harness = await DesignerHarness.Create().OpenAsync();
        await harness.AddSectionAsync("Request", "request", 2);

        harness.Designer.SelectedPaletteType = null;
        var (handled, result) = await harness.Designer.HandleGestureAsync("Ctrl+I");

        Assert.True(handled);
        Assert.False(result.Ok);
        Assert.Contains("palette", harness.Designer.StatusText, StringComparison.OrdinalIgnoreCase);
        Assert.DoesNotContain(harness.IssuedOps, op => op.StartsWith("add_field", StringComparison.Ordinal));
    }
}
