using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

/// <summary>Text entry with a calendar popup: pattern, invalid text, clearing.</summary>
public sealed class SpotDateInputTests
{
    [AvaloniaFact]
    public void ValidTextCommitsWithPattern()
    {
        var input = new SpotDateInput();
        var window = TestHost.Show(input);
        var editor = input.TextBoxControl;
        Assert.NotNull(editor);
        editor!.Focus();
        Dispatcher.UIThread.RunJobs();
        editor.Text = "30-09-2026";
        Assert.True(input.Commit());
        Assert.Equal(new DateOnly(2026, 9, 30), input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void InvalidTextKeepsOldValueAndShowsError()
    {
        var input = new SpotDateInput { Value = new DateOnly(2026, 1, 5) };
        TestHost.Show(input);
        var editor = input.TextBoxControl!;
        editor.Text = "not a date";
        Assert.False(input.Commit());
        Assert.Equal(new DateOnly(2026, 1, 5), input.Value);
        Assert.NotNull(input.ErrorText);
        Assert.Contains("dd-MM-yyyy", input.ErrorText, StringComparison.Ordinal);
        // The field text is restored from the kept value.
        Assert.Equal("05-01-2026", editor.Text);
    }

    [AvaloniaFact]
    public void CustomPatternIsHonoured()
    {
        var input = new SpotDateInput { Pattern = "yyyy/MM/dd" };
        TestHost.Show(input);
        var editor = input.TextBoxControl!;
        editor.Text = "2026/09/30";
        Assert.True(input.Commit());
        Assert.Equal(new DateOnly(2026, 9, 30), input.Value);
        // Display follows the pattern.
        Assert.Equal("2026/09/30", editor.Text);
        // And the default pattern would reject this text.
        var strict = new SpotDateInput();
        TestHost.Show(strict);
        strict.TextBoxControl!.Text = "2026/09/30";
        Assert.False(strict.Commit());
    }

    [AvaloniaFact]
    public void ClearingEmptyTextCommitsNull()
    {
        var input = new SpotDateInput { Value = new DateOnly(2026, 9, 30) };
        TestHost.Show(input);
        var editor = input.TextBoxControl!;
        Assert.Equal("30-09-2026", editor.Text);
        editor.Text = string.Empty;
        Assert.True(input.Commit());
        Assert.Null(input.Value);
        Assert.Null(input.ErrorText);
    }

    [AvaloniaFact]
    public void CalendarButtonIsFocusableAndDrawsAVectorIconNotAnEmoji()
    {
        var input = new SpotDateInput();
        TestHost.Show(input);
        var descendants = Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(input).ToList();
        var button = descendants.OfType<Button>().Single(b => b.Name == "PART_CalendarButton");
        Assert.True(button.Focusable);
        Assert.IsType<Avalonia.Controls.Shapes.Path>(button.Content);
        foreach (var text in descendants.OfType<TextBlock>())
        {
            Assert.DoesNotContain(text.Text ?? string.Empty, c => char.IsSurrogate(c) || (c >= '\u2600' && c <= '\u27BF'));
        }
    }

    [AvaloniaFact]
    public void CalendarButtonTogglesPopupAndSelectionCommits()
    {
        var input = new SpotDateInput();
        var window = TestHost.Show(input);
        var popup = input.CalendarPopup;
        Assert.NotNull(popup);
        Assert.False(popup!.IsOpen);

        input.IsCalendarOpen = true;
        Dispatcher.UIThread.RunJobs();
        Assert.True(popup.IsOpen);

        // Pick a date through the calendar, the way a click would.
        var calendar = popup.Child is Avalonia.Visual childVisual ? FindCalendar(childVisual) : null;
        Assert.NotNull(calendar);
        calendar!.SelectedDate = new DateTime(2026, 9, 30);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(new DateOnly(2026, 9, 30), input.Value);
        Assert.False(input.IsCalendarOpen);
        Assert.Equal("30-09-2026", input.TextBoxControl!.Text);
    }

    private static Calendar? FindCalendar(Avalonia.Visual visual)
    {
        foreach (var child in Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(visual))
        {
            if (child is Calendar calendar)
            {
                return calendar;
            }
        }

        return null;
    }
}
