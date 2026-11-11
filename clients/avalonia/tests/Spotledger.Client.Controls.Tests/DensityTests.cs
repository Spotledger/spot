using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Controls;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

/// <summary>Density.Mode switches control metrics between the token sets.</summary>
public sealed class DensityTests
{
    [AvaloniaTheory]
    [InlineData(DensityMode.Comfortable, 8d)]
    [InlineData(DensityMode.Compact, 4d)]
    public void TextInputPaddingFollowsTheDensityTokens(DensityMode mode, double expectedPadding)
    {
        var input = new SpotTextInput { Placeholder = "name" };
        var host = new ContentControl { Content = input };
        Density.SetMode(host, mode);
        TestHost.Show(host);
        Dispatcher.UIThread.RunJobs();

        var editor = input.TextBoxControl;
        Assert.NotNull(editor);
        Assert.Equal(new Thickness(expectedPadding), editor!.Padding);
    }

    [AvaloniaFact]
    public void TextInputMeasuredHeightIsSmallerInCompact()
    {
        var comfortableInput = new SpotTextInput { Placeholder = "name" };
        var comfortableHost = new ContentControl { Content = comfortableInput };
        Density.SetMode(comfortableHost, DensityMode.Comfortable);
        var comfortableWindow = TestHost.Show(comfortableHost);

        var compactInput = new SpotTextInput { Placeholder = "name" };
        var compactHost = new ContentControl { Content = compactInput };
        Density.SetMode(compactHost, DensityMode.Compact);
        TestHost.Show(compactHost);
        Dispatcher.UIThread.RunJobs();

        var comfortableEditor = comfortableInput.TextBoxControl!;
        var compactEditor = compactInput.TextBoxControl!;
        Assert.True(compactEditor.Bounds.Height < comfortableEditor.Bounds.Height,
            $"compact {compactEditor.Bounds.Height} vs comfortable {comfortableEditor.Bounds.Height}");

        // The difference must be exactly twice the padding token difference (8-4=4 each side).
        var paddingGap = TokenDouble(Tokens.TokenDensityComfortableFormControlPaddingY)
                         - TokenDouble(Tokens.TokenDensityCompactFormControlPaddingY);
        Assert.Equal(2 * paddingGap, comfortableEditor.Bounds.Height - compactEditor.Bounds.Height, 6);
    }

    [AvaloniaFact]
    public void ModeIsInheritedAndDefaultsToComfortable()
    {
        var panel = new StackPanel();
        var input = new SpotTextInput();
        panel.Children.Add(input);
        TestHost.Show(panel);
        Assert.Equal(DensityMode.Comfortable, Density.GetMode(input));

        Density.SetMode(panel, DensityMode.Compact);
        Dispatcher.UIThread.RunJobs();
        Assert.Equal(DensityMode.Compact, Density.GetMode(input));
        Assert.Contains(Density.CompactClass, input.Classes);

        // The class mirrors back off when the container returns to Comfortable.
        Density.SetMode(panel, DensityMode.Comfortable);
        Dispatcher.UIThread.RunJobs();
        Assert.DoesNotContain(Density.CompactClass, input.Classes);
    }

    private static double TokenDouble(string key)
    {
        var app = Avalonia.Application.Current!;
        Assert.True(app.TryGetResource(key, app.ActualThemeVariant, out var value));
        return Assert.IsType<double>(value);
    }
}
