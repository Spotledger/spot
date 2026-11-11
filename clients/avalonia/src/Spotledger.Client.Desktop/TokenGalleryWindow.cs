using Avalonia;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Styling;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Desktop;

internal sealed class TokenGalleryWindow : Window
{
    private readonly StackPanel _content = new() { Spacing = 24, Margin = new Thickness(24) };
    private readonly DockPanel _layout = new();
    private ThemeVariant _themeVariant = ThemeVariant.Light;
    private bool _rightToLeft;

    public TokenGalleryWindow()
    {
        Title = "Spotledger design token gallery";
        Width = 1100;
        Height = 820;
        BuildLayout();
        RebuildContent();
    }

    private void BuildLayout()
    {
        var toolbar = new StackPanel
        {
            Orientation = Orientation.Horizontal,
            Spacing = 8,
            Margin = new Thickness(16)
        };

        var themeButton = new Button { Content = "Toggle light / dark" };
        themeButton.Click += (_, _) =>
        {
            _themeVariant = _themeVariant == ThemeVariant.Light ? ThemeVariant.Dark : ThemeVariant.Light;
            if (Application.Current is { } application)
            {
                application.RequestedThemeVariant = _themeVariant;
            }

            RebuildContent();
        };

        var directionButton = new Button { Content = "Toggle LTR / RTL" };
        directionButton.Click += (_, _) =>
        {
            _rightToLeft = !_rightToLeft;
            RebuildContent();
        };

        toolbar.Children.Add(themeButton);
        toolbar.Children.Add(directionButton);
        DockPanel.SetDock(toolbar, Dock.Top);
        _layout.Children.Add(toolbar);
        _layout.Children.Add(new ScrollViewer { Content = _content });
        Content = _layout;
    }

    private void RebuildContent()
    {
        _content.Children.Clear();
        _content.FlowDirection = _rightToLeft ? FlowDirection.RightToLeft : FlowDirection.LeftToRight;
        Background = new SolidColorBrush(GetColor(Tokens.TokenColorSurfaceCanvas));

        AddSectionTitle($"Color swatches — {(_themeVariant == ThemeVariant.Light ? "Light" : "Dark")}");
        AddSwatches();
        AddSectionTitle("4 px spacing scale");
        AddSpacingScale();
        AddSectionTitle("Type ramp and Urdu fallback");
        AddTypeRamp();
        AddSectionTitle("Density examples");
        AddDensityExamples();
    }

    private void AddSwatches()
    {
        var rows = new WrapPanel { Orientation = Orientation.Horizontal };
        var swatches = new (string Label, string Key)[]
        {
            ("Brand", Tokens.TokenColorBrand500),
            ("Success", Tokens.TokenColorSemanticSuccess),
            ("Warning", Tokens.TokenColorSemanticWarning),
            ("Danger", Tokens.TokenColorSemanticDanger),
            ("Info", Tokens.TokenColorSemanticInfo),
            ("Canvas", Tokens.TokenColorSurfaceCanvas),
            ("Raised", Tokens.TokenColorSurfaceRaised),
            ("Border", Tokens.TokenColorBorderDefault)
        };

        foreach (var (label, key) in swatches)
        {
            var item = new StackPanel { Width = 132, Spacing = 4, Margin = new Thickness(0, 0, 12, 12) };
            item.Children.Add(new Border
            {
                Height = 60,
                CornerRadius = new CornerRadius(6),
                BorderThickness = new Thickness(1),
                BorderBrush = new SolidColorBrush(GetColor(Tokens.TokenColorBorderDefault)),
                Background = new SolidColorBrush(GetColor(key))
            });
            item.Children.Add(new TextBlock
            {
                Text = label,
                Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
            });
            rows.Children.Add(item);
        }

        _content.Children.Add(rows);
    }

    private void AddSpacingScale()
    {
        var scale = new StackPanel { Spacing = 4 };
        for (var index = 0; index <= 12; index++)
        {
            var width = GetDouble($"Space.{index}");
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, VerticalAlignment = VerticalAlignment.Center };
            row.Children.Add(new TextBlock
            {
                Text = $"{index}: {width:0} px",
                Width = 90,
                Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
            });
            row.Children.Add(new Border
            {
                Width = width,
                Height = 10,
                Background = new SolidColorBrush(GetColor(Tokens.TokenColorBrand500))
            });
            scale.Children.Add(row);
        }

        _content.Children.Add(scale);
    }

    private void AddTypeRamp()
    {
        var typeRamp = new StackPanel { Spacing = 8 };
        var samples = new (string Label, string SizeKey)[]
        {
            ("Caption", Tokens.TokenFontSizeCaption),
            ("Body", Tokens.TokenFontSizeBody),
            ("Comfortable", Tokens.TokenFontSizeComfortable),
            ("Title", Tokens.TokenFontSizeTitle),
            ("Heading", Tokens.TokenFontSizeHeading),
            ("Display", Tokens.TokenFontSizeDisplay)
        };
        foreach (var (label, sizeKey) in samples)
        {
            typeRamp.Children.Add(new TextBlock
            {
                Text = $"{label} — Spotledger 123",
                FontFamily = GetValue<FontFamily>(Tokens.TokenFontFamilyLatin),
                FontSize = GetDouble(sizeKey),
                Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
            });
        }

        typeRamp.Children.Add(new TextBlock
        {
            Text = "السلام علیکم — کمپنی کا دفتر",
            FontFamily = GetValue<FontFamily>(Tokens.TokenFontFamilyUrdu),
            FontSize = GetDouble(Tokens.TokenFontSizeTitle),
            FlowDirection = FlowDirection.RightToLeft,
            Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
        });
        _content.Children.Add(typeRamp);
    }

    private void AddDensityExamples()
    {
        var densityRows = new StackPanel { Spacing = 12 };
        AddDensityRow(densityRows, "Compact", Tokens.TokenDensityCompactGridRowHeight, Tokens.TokenDensityCompactGridCellPadding,
            Tokens.TokenDensityCompactFormFieldGap, Tokens.TokenDensityCompactFormControlPaddingY);
        AddDensityRow(densityRows, "Comfortable", Tokens.TokenDensityComfortableGridRowHeight, Tokens.TokenDensityComfortableGridCellPadding,
            Tokens.TokenDensityComfortableFormFieldGap, Tokens.TokenDensityComfortableFormControlPaddingY);
        _content.Children.Add(densityRows);
    }

    private void AddDensityRow(StackPanel parent, string name, string rowHeightKey, string cellPaddingKey, string fieldGapKey, string controlPaddingKey)
    {
        var rowHeight = GetDouble(rowHeightKey);
        var cellPadding = GetDouble(cellPaddingKey);
        var fieldGap = GetDouble(fieldGapKey);
        var controlPadding = GetDouble(controlPaddingKey);
        var example = new StackPanel { Spacing = fieldGap };
        example.Children.Add(new TextBlock
        {
            Text = $"{name}: grid row {rowHeight:0}px · cell padding {cellPadding:0}px · form gap {fieldGap:0}px · control padding {controlPadding:0}px",
            Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
        });
        example.Children.Add(new Border
        {
            Height = rowHeight,
            Padding = new Thickness(cellPadding),
            Background = new SolidColorBrush(GetColor(Tokens.TokenColorSurfaceRaised)),
            Child = new TextBlock
            {
                Text = "Invoice row — 4 items — PKR 12,400",
                VerticalAlignment = VerticalAlignment.Center,
                Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
            }
        });
        parent.Children.Add(example);
    }

    private void AddSectionTitle(string title)
    {
        _content.Children.Add(new TextBlock
        {
            Text = title,
            FontSize = GetDouble(Tokens.TokenFontSizeTitle),
            FontWeight = GetValue<FontWeight>(Tokens.TokenFontWeightSemibold),
            Foreground = new SolidColorBrush(GetColor(Tokens.TokenColorTextPrimary))
        });
    }

    private Color GetColor(string key) => GetValue<Color>(key);

    private double GetDouble(string key) => GetValue<double>(key);

    private T GetValue<T>(string key)
    {
        if (Application.Current is { } application && application.TryGetResource(key, _themeVariant, out var value) && value is T typed)
        {
            return typed;
        }

        throw new KeyNotFoundException($"Theme token '{key}' is missing or has an unexpected type.");
    }
}
