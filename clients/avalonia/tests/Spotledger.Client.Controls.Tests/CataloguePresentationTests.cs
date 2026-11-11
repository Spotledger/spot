using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class CataloguePresentationTests
{
    [AvaloniaFact]
    public void GalleryIncludesEveryNewControlAndStatesInBothDensitiesAndDirections()
    {
        var gallery = new ControlGallery();
        var window = TestHost.Show(gallery, 1000, 2400);
        var controls = gallery.GetVisualDescendants().OfType<Control>().ToArray();
        foreach (var type in new[] { typeof(SpotLookupBox), typeof(SpotMultiSelect), typeof(SpotSecretInput), typeof(SpotTextArea), typeof(SpotTimeInput), typeof(SpotDateTimeInput), typeof(SpotPercentInput), typeof(SpotAttachControl), typeof(SpotTableControl) })
            Assert.True(controls.Count(x => x.GetType() == type) >= 4, type.Name);
        Assert.Contains(controls.OfType<SpotLookupBox>(), x => x.IsLoading);
        Assert.Contains(controls.OfType<SpotLookupBox>(), x => x.IsStale);
        Assert.Contains(controls.OfType<SpotLookupBox>(), x => x.ErrorText is not null);
        Assert.Contains(controls.OfType<SpotAttachControl>(), x => x.IsUploading);
        Assert.Contains(controls.OfType<SpotAttachControl>(), x => x.ErrorText is not null);
        window.Close();
    }

    [AvaloniaTheory]
    [InlineData("lookup", "SpotLookupBox|RightToLeft|True|گاہک|اکمی")]
    [InlineData("multi", "SpotMultiSelect|RightToLeft|True|گاہک|اکمی, کریم")]
    [InlineData("secret", "SpotSecretInput|RightToLeft|True|گاہک|Set")]
    public void UrduRtlSnapshotHasMirroredLayoutSelectableTextAndAutomation(string kind, string expected)
    {
        CatalogueControl input = kind switch
        {
            "lookup" => new SpotLookupBox { SelectedItem = new("a", "اکمی"), IsReadOnly = true },
            "multi" => new SpotMultiSelect { SelectedItems = [new("a", "اکمی"), new("b", "کریم")], IsReadOnly = true },
            _ => new SpotSecretInput { Value = new(true), IsReadOnly = true },
        };
        AutomationProperties.SetName(input, "گاہک");
        var field = new SpotField { Label = "گاہک", Content = input, WideBreakpoint = 100, FlowDirection = FlowDirection.RightToLeft, Width = 600 };
        var window = TestHost.Show(field, 700);
        Dispatcher.UIThread.RunJobs();
        var display = input.GetVisualDescendants().OfType<TextBox>().First(x => x.IsVisible && x.IsReadOnly);
        Assert.True(display.Bounds.Width > 0);
        Assert.NotNull(ControlAutomationPeer.CreatePeerForElement(input));
        var mirror = (bool)typeof(Avalonia.Visual).GetProperty("HasMirrorTransform")!.GetValue(field)!;
        var snapshot = $"{input.GetType().Name}|{input.FlowDirection}|{mirror}|{AutomationProperties.GetName(input)}|{display.Text}";
        Assert.Equal(expected, snapshot);
        var label = field.GetVisualDescendants().OfType<Control>().Single(x => x.Name == "PART_InlineLabel");
        Assert.True(label.PointToScreen(default).X > input.PointToScreen(default).X);
        Avalonia.Media.Imaging.Bitmap? frame = null;
        for (var attempt = 0; attempt < 40 && frame is null; attempt++)
        {
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
            frame = window.CaptureRenderedFrame();
        }
        Assert.NotNull(frame);
        Assert.Equal(700, frame.PixelSize.Width);
        var output = Path.Combine(AppContext.BaseDirectory, "snapshots", "catalogue");
        Directory.CreateDirectory(output);
        File.WriteAllText(Path.Combine(output, kind + "-urdu-rtl.layout.txt"), snapshot);
        using (frame) frame.Save(Path.Combine(output, kind + "-urdu-rtl.png"), new Avalonia.Media.Imaging.PngBitmapEncoderOptions());
        window.Close();
    }
}
