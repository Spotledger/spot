using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.VisualTree;
using Spotledger.Client.Extensions;
using Spotledger.Client.Extensions.Renderer;
using Xunit;

namespace Spotledger.Client.Extensions.Tests;

/// <summary>The renderer's tokens really come from the theme: the panel card's border,
/// surface and padding change with the variant, and a dark window is not drawn light.
/// (A plain <c>TryFindResource(key)</c> reads the Default variant only, which is exactly how
/// a themed surface silently renders unthemed.)</summary>
public sealed class ThemeTests
{
    [AvaloniaTheory]
    [InlineData(false, "#F8FAFC", "#CBD5E1")]
    [InlineData(true, "#111827", "#475569")]
    public void ThePanelCardTakesItsSurfaceAndBorderFromTheActiveVariant(bool dark, string surface, string border)
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", new ExtensionServices(Queries: FakeQueries.Sample()));
        SidePanelHost host = new();
        host.Show(view);
        Window window = TestHost.Show(host, 400, 560, dark: dark);
        try
        {
            Assert.Equal(dark ? ThemeVariant.Dark : ThemeVariant.Light, host.ActualThemeVariant);
            Border card = Descendants(host).OfType<Border>().First(item => item.Child is StackPanel);
            Assert.Equal(new Avalonia.Thickness(1), card.BorderThickness);
            Assert.Equal(Color.Parse(border), Assert.IsType<SolidColorBrush>(card.BorderBrush).Color);
            Assert.Equal(Color.Parse(surface), Assert.IsType<SolidColorBrush>(card.Background).Color);
            Assert.True(card.Padding.Left > 0, "the card's padding comes from a spacing token");
        }
        finally
        {
            window.Close();
        }
    }

    [AvaloniaFact]
    public void TheTrackedDensityChangesTheGapBetweenBlocks()
    {
        ExtensionSet set = Sample.Set();
        using ExtensionContext context = Sample.ProjectContext();
        using SlotView view = set.ForSlot(ExtensionSlotCatalog.DocSidepanel, context, "project", new ExtensionServices(Queries: FakeQueries.Sample()));
        SidePanelHost host = new();
        host.Show(view);
        Window window = TestHost.Show(host, 400, 560);
        try
        {
            StackPanel body = Assert.IsType<StackPanel>(host.Content);
            double comfortable = body.Spacing;
            Spotledger.Client.Controls.Density.SetMode(host, Spotledger.Client.Controls.DensityMode.Compact);
            double compact = Assert.IsType<StackPanel>(host.Content).Spacing;
            Assert.True(comfortable > compact, $"comfortable {comfortable} should exceed compact {compact}");
            Assert.Equal(8d, compact);
        }
        finally
        {
            window.Close();
        }
    }

    private static IEnumerable<Visual> Descendants(Visual root)
    {
        foreach (Visual child in root.GetVisualChildren())
        {
            yield return child;
            foreach (Visual descendant in Descendants(child))
            {
                yield return descendant;
            }
        }
    }
}
