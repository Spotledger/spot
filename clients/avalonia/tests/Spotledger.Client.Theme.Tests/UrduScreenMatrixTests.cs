using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Grid;
using Spotledger.Client.TestSupport;
using Spotledger.Client.Theme;
using Xunit;

namespace Spotledger.Client.Theme.Tests;

/// <summary>
/// The Urdu right-to-left screen matrix: the controls and screens that exist today, rendered in
/// Urdu with the bundled Naskh face, in Light and Dark at both densities, plus the two
/// high-contrast palettes for the field controls. Baselines live in <c>Baselines/</c> and are
/// compared by the shared <see cref="ScreenshotAssert"/> matrix (WP-UI-16.T1, variant dimension
/// from WP-UI-16.T3): the variant is one matrix dimension, so a case keeps its exact file name
/// and a mismatch writes real actual/expected/diff PNGs and a per-case HTML page for review.
///
/// The shell chrome case is not in this set: <c>ShellView</c> needs a <c>ShellViewModel</c> over an
/// <c>ApiConnection</c> with the Shell lane's fake server, which lives in the Shell test project
/// (not owned here). See the task's open questions.
/// </summary>
public sealed class UrduScreenMatrixTests
{
    private const string CustomerName = "احمد رضا";
    private const string FormHeading = "نئی رسید";
    private const string GridHeaderItem = "شے کا کوڈ";
    private const int FieldWidth = 480;
    private const int FieldHeight = 140;

    /// <summary>This project's committed baselines; an ordinary run only ever reads them.</summary>
    private static readonly string Baselines =
        Path.GetFullPath(Path.Combine(AppContext.BaseDirectory, "..", "..", "..", "Baselines"));

    /// <summary>
    /// One Urdu matrix case for the shared helper. The variant decides the palette (and therefore
    /// the file stem) and must agree with the dark flag.
    /// </summary>
    private static ScreenshotMatrix Case(int width, int height, ThemeVariantKind variant, DensityMode density) =>
        new(Baselines, width, height, density, dark: variant.IsDark(), rtl: true, Variant: variant);

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void FieldControlsRenderInUrduRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-text", Field("گاہک کا نام", new SpotTextInput { Text = CustomerName }), Case(FieldWidth, FieldHeight, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
            Assert.Equal(density, Density.GetMode((Visual)window.Content!));
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void DecimalFieldRendersTheAmountRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-decimal", Field("رقم", new SpotDecimalInput { Value = 1234.50m, Scale = 2 }), Case(FieldWidth, FieldHeight, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    /// <summary>
    /// The date field in pixels: the calendar affordance is vector geometry, so the rendering no
    /// longer depends on the host emoji font. The control shows the date as Latin digits
    /// (dd-MM-yyyy) in every language today; it does not apply the user's numeral setting.
    /// </summary>
    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void TheDateFieldRendersItsDateRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-date",
            Field("تاریخ", new SpotDateInput { Value = new DateOnly(2026, 10, 28) }), Case(FieldWidth, FieldHeight, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.HighContrast)]
    [InlineData(ThemeVariantKind.HighContrastDark)]
    public void TheDateFieldRendersInHighContrast(ThemeVariantKind variant)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-date-high-contrast",
            Field("تاریخ", new SpotDateInput { Value = new DateOnly(2026, 10, 28) }), Case(FieldWidth, FieldHeight, variant, DensityMode.Comfortable));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void SelectFieldRendersUrduOptionsRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var select = new SpotSelect
        {
            Options =
            [
                new SelectOption("cash", "نقد"),
                new SelectOption("bank", "بینک"),
                new SelectOption("cheque", "چیک"),
            ],
        };
        select.SelectedIndex = 1;

        var window = ScreenshotAssert.MatchesWindow("urdu-field-select", Field("ادائیگی کا طریقہ", select), Case(FieldWidth, FieldHeight, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void CheckBoxRendersItsUrduLabelRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-checkbox",
            Field("منظوری", new SpotCheckBox { Content = "منظور شدہ", IsChecked = true }), Case(FieldWidth, FieldHeight, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void AGeneratedFormRendersItsUrduLabelSet(ThemeVariantKind variant, DensityMode density)
    {
        var form = Form();
        var window = ScreenshotAssert.MatchesWindow("urdu-form", form, Case(640, 420, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
            var fields = form.Children.OfType<SpotField>().ToArray();
            Assert.Equal(5, fields.Length);
            Assert.All(fields, field => Assert.False(string.IsNullOrWhiteSpace(field.Label)));
            Assert.All(fields, field => Assert.Equal(FlowDirection.RightToLeft, field.FlowDirection));
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    /// <summary>
    /// A real mirrored-layout assertion (not just an RTL file name): the field label and its input
    /// swap sides between directions, compared through screen coordinates of the laid-out visuals.
    /// </summary>
    [AvaloniaFact]
    public void TheFieldLabelAndInputSwapSidesBetweenDirections()
    {
        foreach (var direction in new[] { FlowDirection.LeftToRight, FlowDirection.RightToLeft })
        {
            var input = new SpotTextInput { Text = CustomerName };
            var field = new SpotField { Label = "گاہک کا نام", Content = input, Width = 600 };
            var window = new Window { Width = 640, Height = 140, FlowDirection = direction, Content = field };
            try
            {
                window.Show();
                window.UpdateLayout();
                Avalonia.Threading.Dispatcher.UIThread.RunJobs();

                var label = field.GetVisualDescendants().OfType<TextBlock>().First(text => text.Text == "گاہک کا نام");
                var labelX = label.PointToScreen(default).X;
                var inputX = input.PointToScreen(default).X;

                Assert.Equal(direction == FlowDirection.RightToLeft, labelX > inputX);
            }
            finally
            {
                window.Close();
            }
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.Light, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Light, DensityMode.Compact)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Comfortable)]
    [InlineData(ThemeVariantKind.Dark, DensityMode.Compact)]
    public void TheGridRendersAnUrduHeaderAndMixedRowsRightToLeft(ThemeVariantKind variant, DensityMode density)
    {
        var grid = MixedGrid();
        var window = ScreenshotAssert.MatchesWindow("urdu-grid", grid, Case(720, 260, variant, density));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
            // The pixels show drawn rows, not loading placeholders (WP-UI-15.T6; reviewed PNGs).
            Assert.Equal(4, grid.RowCount);
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    /// <summary>
    /// The grid's Urdu header and mixed rows, asserted structurally: it guards the header and the
    /// mixed content independently of the pixel baselines above.
    /// </summary>
    [AvaloniaFact]
    public void TheGridHasAnUrduHeaderAndMixedRows()
    {
        var grid = MixedGrid();
        var window = new Window
        {
            Width = 720,
            Height = 260,
            Content = grid,
            FlowDirection = FlowDirection.RightToLeft,
        };
        try
        {
            window.Show();
            window.UpdateLayout();
            Avalonia.Threading.Dispatcher.UIThread.RunJobs();
            Avalonia.Headless.AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Avalonia.Threading.Dispatcher.UIThread.RunJobs();

            Assert.Equal(4, grid.RowCount);
            Assert.Equal(FlowDirection.RightToLeft, grid.FlowDirection);
            Assert.Equal(GridHeaderItem, grid.Columns![0].Header);
            Assert.All(grid.Columns!, column => Assert.True(BidiText.HasArabicScript(column.Header)));
        }
        finally
        {
            window.Close();
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.HighContrast)]
    [InlineData(ThemeVariantKind.HighContrastDark)]
    public void FieldControlsRenderInHighContrast(ThemeVariantKind variant)
    {
        var window = ScreenshotAssert.MatchesWindow("urdu-field-text-high-contrast",
            Field("گاہک کا نام", new SpotTextInput { Text = CustomerName }), Case(FieldWidth, FieldHeight, variant, DensityMode.Comfortable));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, window.FlowDirection);
            Assert.Equal(variant, ThemeVariants.FromThemeVariant(window.RequestedThemeVariant));
        }
        finally
        {
            ScreenshotAssert.Close(window);
        }
    }

    [AvaloniaTheory]
    [InlineData(ThemeVariantKind.HighContrast)]
    [InlineData(ThemeVariantKind.HighContrastDark)]
    public void TheDecimalAndCheckBoxRenderInHighContrast(ThemeVariantKind variant)
    {
        var decimalWindow = ScreenshotAssert.MatchesWindow("urdu-field-decimal-high-contrast",
            Field("رقم", new SpotDecimalInput { Value = 9875.25m }), Case(FieldWidth, FieldHeight, variant, DensityMode.Comfortable));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, decimalWindow.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(decimalWindow);
        }

        var checkWindow = ScreenshotAssert.MatchesWindow("urdu-field-checkbox-high-contrast",
            Field("منظوری", new SpotCheckBox { Content = "منظور شدہ", IsChecked = true }), Case(FieldWidth, FieldHeight, variant, DensityMode.Comfortable));
        try
        {
            Assert.Equal(FlowDirection.RightToLeft, checkWindow.FlowDirection);
        }
        finally
        {
            ScreenshotAssert.Close(checkWindow);
        }
    }

    private static Border Field(string label, Control content) => new()
    {
        Padding = new Thickness(12),
        Child = new SpotField { Label = label, Content = content },
    };

    private static StackPanel Form()
    {
        var panel = new StackPanel
        {
            Spacing = 12,
            Margin = new Thickness(16),
            Children =
            {
                new TextBlock { Text = FormHeading, FontFamily = SpotledgerFonts.Urdu },
                new SpotField { Label = "گاہک کا نام", Content = new SpotTextInput { Text = CustomerName } },
                new SpotField { Label = "رسید نمبر", Content = new SpotTextInput { Text = "INV-2026-0042" } },
                new SpotField { Label = "رقم", Content = new SpotDecimalInput { Value = 9875.25m, Scale = 2 } },
                new SpotField
                {
                    Label = "ادائیگی کا طریقہ",
                    Content = new SpotSelect
                    {
                        Options = [new SelectOption("cash", "نقد"), new SelectOption("bank", "بینک")],
                        SelectedIndex = 0,
                    },
                },
                new SpotField { Label = "منظوری", Content = new SpotCheckBox { Content = "منظور شدہ", IsChecked = true } },
            },
        };

        return panel;
    }

    private static SpotGrid MixedGrid()
    {
        var grid = new SpotGrid
        {
            Columns =
            [
                new GridColumnModel { Header = GridHeaderItem, Width = GridColumnWidth.Pixels(160) },
                new GridColumnModel { Header = "مقدار", Width = GridColumnWidth.Pixels(100) },
                new GridColumnModel { Header = "رقم", Width = GridColumnWidth.Pixels(140) },
            ],
            Source = new InMemoryGridSource(
            [
                GridRow.Of("r1",
                    new GridCell("SKU-1234-A"), new GridCell("12"), new GridCell("1,234.50")),
                GridRow.Of("r2",
                    new GridCell("SKU-77-B"), new GridCell("4"), new GridCell("987.25")),
                GridRow.Of("r3",
                    new GridCell("SKU-9910-C"), new GridCell("144"), new GridCell("58,400.00")),
                GridRow.Of("r4",
                    new GridCell("SKU-5507-D"), new GridCell("9"), new GridCell("2,475.00")),
            ]),
        };

        return grid;
    }

}
