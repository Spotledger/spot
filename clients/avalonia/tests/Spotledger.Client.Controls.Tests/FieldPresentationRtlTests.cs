using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class FieldPresentationRtlTests
{
    [AvaloniaFact]
    public void GalleryCarriesPresentationStatesInEveryDirectionAndDensity()
    {
        var gallery = new ControlGallery();
        var window = TestHost.Show(gallery, 1000, 2400);
        var fixtures = gallery.GetVisualDescendants().OfType<FieldPresentationGallery>().ToArray();
        Assert.Equal(4, fixtures.Length);
        foreach (var fixture in fixtures)
        {
            var controls = fixture.GetVisualDescendants().OfType<Control>().ToArray();
            Assert.Equal(2, controls.OfType<SpotMaskedField>().Count());
            Assert.Contains(controls.OfType<SpotMaskedField>(), x => x.CanReveal);
            Assert.Contains(controls.OfType<SpotMaskedField>(), x => !x.CanReveal);
            Assert.Contains(controls.OfType<SpotCurrencyInput>(), x => x.CurrencyInfo is null);
            Assert.Contains(controls.OfType<SpotCurrencyInput>(), x => x.IsReadOnly);
            Assert.Contains(controls.OfType<SpotCurrencyInput>(), x => x.ErrorText is not null);
            Assert.Contains(controls.OfType<SpotAttachControl>(), x => x.ErrorText?.Contains("3 bytes", StringComparison.Ordinal) == true);
            Assert.Contains(controls.OfType<SpotAttachControl>(), x => x.ErrorText?.Contains("text/plain", StringComparison.Ordinal) == true);
            Assert.Contains(controls.OfType<SpotLookupBox>(), x => x.Columns.Count == 2 && x.CanCreateTarget);
            Assert.Contains(controls.OfType<SpotMultiSelect>(), x => x.Lookup.Columns.Count == 2 && x.CanCreateTarget);
            Assert.Contains(controls.OfType<SpotSelectMultiple>(), x => x.SelectedValues.Count == 2 && x.IsReadOnly);
        }
        window.Close();
    }

    [AvaloniaTheory]
    [InlineData(FlowDirection.LeftToRight)]
    [InlineData(FlowDirection.RightToLeft)]
    public async Task PresentationRasterAndColumnPositionsFollowDirection(FlowDirection direction)
    {
        var lookup = new SpotLookupBox { Source = new ColumnLookupSource(), Delay = new ImmediateLookupDelay() };
        var field = new Spotledger.Client.Metadata.FieldModel("value", "Protected", Spotledger.Client.Metadata.FieldKind.Text);
        var draft = FieldPresentationTests.Draft(field, masked: true);
        var masked = FieldControlFactory.Create(field, draft.Fields["value"]);
        var panel = new StackPanel { FlowDirection = direction, Width = 600 };
        panel.Children.Add(masked);
        panel.Children.Add(new SpotCurrencyInput { CurrencyInfo = new("PKR", "Rs"), Text = "123.45" });
        panel.Children.Add(new SpotSelectMultiple { Options = ["One", "Two"], SelectedValues = ["Two"] });
        panel.Children.Add(lookup);
        var window = TestHost.Show(panel, 700, 700);
        lookup.SearchText = "typed";
        await lookup.PendingSearch;
        window.UpdateLayout();
        Dispatcher.UIThread.RunJobs();
        var code = lookup.GetVisualDescendants().OfType<TextBlock>().Single(text => text.Text == "Code");
        var region = lookup.GetVisualDescendants().OfType<TextBlock>().Single(text => text.Text == "Region");
        Assert.True(code.Bounds.Width > 0 && region.Bounds.Width > 0);
        Assert.Equal(direction == FlowDirection.RightToLeft, code.PointToScreen(default).X > region.PointToScreen(default).X);
        Assert.Equal(direction, masked.FlowDirection);
        Assert.Equal(direction, lookup.FlowDirection);
        Avalonia.Media.Imaging.Bitmap? frame = null;
        for (var attempt = 0; attempt < 40 && frame is null; attempt++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            frame = window.CaptureRenderedFrame();
        }
        Assert.NotNull(frame);
        var output = Path.Combine(AppContext.BaseDirectory, "snapshots", "field-presentation");
        Directory.CreateDirectory(output);
        using (frame)
        {
            Assert.Equal(700, frame.PixelSize.Width);
            frame.Save(Path.Combine(output, direction + ".png"), new Avalonia.Media.Imaging.PngBitmapEncoderOptions());
        }
        File.WriteAllText(Path.Combine(output, direction + ".layout.txt"), $"{direction}|{code.Text}|{region.Text}|{lookup.Items[0].Label}");
        window.Close();
    }
}
