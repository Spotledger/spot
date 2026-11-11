using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia.Controls;
using Avalonia.Controls.Presenters;
using Avalonia.Controls.Primitives;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Logging;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

/// <summary>SpotCheckBox and SpotSelect over SelectOption lists.</summary>
public sealed class SpotCheckBoxSelectTests
{
    private static readonly IReadOnlyList<SelectOption> Customers = new List<SelectOption>
    {
        new("cust-1", "Acme Textiles / اکمی ٹیکسٹائلز"),
        new("cust-2", "Karim Traders / کریم ٹریڈرز"),
        new("cust-3", "Lahore Mills / لاہور ملز"),
    }.AsReadOnly();

    [AvaloniaFact]
    public void CheckBoxTogglesIsChecked()
    {
        var box = new SpotCheckBox { Content = "تاریخ" };
        TestHost.Show(box);
        Assert.False(box.IsChecked); // CheckBox defaults to false, tri-state is opt-in
        box.IsChecked = true;
        Assert.True(box.IsChecked);
        box.IsChecked = false;
        Assert.False(box.IsChecked);
    }

    [AvaloniaFact]
    public void SelectOptionsPopulateItemsAndSelectionExposesValue()
    {
        var select = new SpotSelect { Options = Customers };
        TestHost.Show(select);
        Dispatcher.UIThread.RunJobs();

        Assert.Equal(3, select.ItemCount);
        select.SelectedIndex = 1;
        var selected = Assert.IsType<SelectOption>(select.SelectedItem);
        Assert.Equal("cust-2", selected.Value);

        // The selection box shows the label, not the record's default rendering.
        Assert.Equal("Karim Traders / کریم ٹریڈرز", selected.ToString());
    }

    [AvaloniaFact]
    public void SelectTypeToSearchIsEnabled()
    {
        var select = new SpotSelect { Options = Customers };
        TestHost.Show(select);
        Assert.True(select.IsTextSearchEnabled);
    }

    [AvaloniaFact]
    public void SelectErrorTextAppliesErrorPseudoClass()
    {
        var select = new SpotSelect { Options = Customers, ErrorText = "required" };
        TestHost.Show(select);
        Dispatcher.UIThread.RunJobs();
        Assert.True(RtlFieldTestsHelpers.HasPseudoClass(select, ":error"));
    }

    [AvaloniaFact]
    public void CheckBoxStylingResolvesNoTemplatePartMissing()
    {
        // The library theme gives the check box a box + glyph template; verify it applied.
        var box = new SpotCheckBox { Content = "ok" };
        TestHost.Show(box);
        Dispatcher.UIThread.RunJobs();
        var presenter = box.GetVisualDescendants().OfType<ContentPresenter>().FirstOrDefault();
        Assert.NotNull(presenter);
        Assert.True(box.Bounds.Width > 0 && box.Bounds.Height > 0);
    }
}

internal static class RtlFieldTestsHelpers
{
    internal static bool HasPseudoClass(Control control, string css)
    {
        var property = typeof(Avalonia.StyledElement).GetProperty("PseudoClasses",
            System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Public);
        var set = property!.GetValue(control)!;
        var contains = set.GetType().GetMethod("Contains", [typeof(string)])!;
        return (bool)contains.Invoke(set, [css])!;
    }
}

/// <summary>
/// The gallery renders headlessly in both theme variants with zero binding errors,
/// captured through Avalonia's logging sink (area "Binding").
/// </summary>
public sealed class GalleryBindingTests
{
    private sealed class RecordingSink : ILogSink
    {
        public List<string> Errors { get; } = [];

        public bool IsEnabled(LogEventLevel level, string area) =>
            level >= LogEventLevel.Warning && string.Equals(area, "Binding", StringComparison.Ordinal);

        public void Log(LogEventLevel level, string area, object? source, string message) =>
            Errors.Add($"{level}: {message} [{source?.GetType().Name}]");

        public void Log(LogEventLevel level, string area, object? source, string messageTemplate, params object?[] messageProperties)
        {
            var rendered = messageTemplate;
            for (var i = 0; i < messageProperties.Length; i++)
            {
                rendered = rendered.Replace("{" + i + "}", messageProperties[i]?.ToString() ?? "null", StringComparison.Ordinal);
            }

            Errors.Add($"{level}: {rendered} [{source?.GetType().Name}]");
        }
    }

    [AvaloniaTheory]
    [InlineData("Light")]
    [InlineData("Dark")]
    public void GalleryRendersWithoutBindingErrors(string variantName)
    {
        var variant = variantName == "Dark" ? Avalonia.Styling.ThemeVariant.Dark : Avalonia.Styling.ThemeVariant.Light;
        var app = Avalonia.Application.Current!;
        app.RequestedThemeVariant = variant;
        Dispatcher.UIThread.RunJobs();

        var sink = new RecordingSink();
        Logger.Sink = sink;
        try
        {
            var gallery = new ControlGallery { Width = 900 };
            var window = TestHost.Show(gallery, width: 960, height: 2400);
            Dispatcher.UIThread.RunJobs();
            window.Close();
            Dispatcher.UIThread.RunJobs();
        }
        finally
        {
            Logger.Sink = null;
            app.RequestedThemeVariant = null;
        }

        Assert.Empty(sink.Errors);
    }
}
