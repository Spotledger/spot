using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class ScrollViewerThemeTests
{
    [AvaloniaFact]
    public void ScrollViewerWithTheThemeLaysOutAndScrolls()
    {
        var content = new Border { Width = 400, Height = 2000 };
        var viewer = new ScrollViewer { Content = content, Width = 200, Height = 100 };
        var window = new Window { Content = viewer, Width = 300, Height = 200 };
        window.Show();
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(2000, viewer.Extent.Height);
        Assert.Equal(100, viewer.Viewport.Height);
        viewer.Offset = new Avalonia.Vector(0, 50);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(50, viewer.Offset.Y);
    }
}
