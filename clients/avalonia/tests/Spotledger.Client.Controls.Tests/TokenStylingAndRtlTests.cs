using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Xml.Linq;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

/// <summary>
/// The brief's styling rule, enforced mechanically: <c>Controls.axaml</c> may style only
/// through resource references. A literal colour (hex or named), a literal FontFamily, or a
/// numeric Padding/Margin/Height/CornerRadius other than 0 fails the build-visible test.
/// </summary>
public sealed class TokenStylingTests
{
    private static readonly XNamespace AvaloniaNs = "https://github.com/avaloniaui";
    private static readonly XNamespace XamlNs = "http://schemas.microsoft.com/winfx/2006/xaml";

    private static readonly HashSet<string> ThicknessProperties =
        new(StringComparer.Ordinal) { "Padding", "Margin", "Height", "CornerRadius", "BorderThickness" };

    private static readonly HashSet<string> NamedColors =
        new(StringComparer.OrdinalIgnoreCase)
        {
            "Red", "Green", "Blue", "Black", "White", "Gray", "Grey", "Silver", "Yellow",
            "Orange", "Purple", "Pink", "Brown", "Cyan", "Magenta", "Transparent"
        };

    [Fact]
    public void ControlsThemeUsesOnlyTokenReferences()
    {
        var path = LocateTheme();
        var doc = XDocument.Load(path);

        var violations = new List<string>();
        foreach (var element in doc.DescendantNodes().OfType<XElement>())
        {
            // ThemeDictionaries legitimately hold literal Color values; the rule is about
            // *styling* (Setters and template-element attributes), not the token source.
            if (IsInsideThemeDictionaries(element, doc))
            {
                continue;
            }

            foreach (var attribute in element.Attributes())
            {
                var name = attribute.Name.LocalName;
                var value = attribute.Value.Trim();
                if (value.Length == 0 || value.StartsWith('{'))
                {
                    continue; // markup extension: DynamicResource, StaticResource, TemplateBinding, token extensions
                }

                // A Setter's literal lives in @Value and its property in @Property.
                if (name == "Property" && element.Name.LocalName == "Setter")
                {
                    var setterValue = element.Attribute("Value")?.Value?.Trim() ?? string.Empty;
                    if (setterValue.Length == 0 || setterValue.StartsWith('{'))
                    {
                        continue;
                    }

                    name = value;
                    value = setterValue;
                }

                if (name is "Background" or "Foreground" or "BorderBrush" or "CaretBrush"
                    or "SelectionBrush" or "Stroke" or "Fill")
                {
                    if (LooksLikeColor(value))
                    {
                        violations.Add($"{Local(element)}: {name}=\"{value}\" is a literal colour");
                    }
                }
                else if (name == "FontFamily")
                {
                    violations.Add($"{Local(element)}: FontFamily=\"{value}\" is a literal font");
                }
                else if (ThicknessProperties.Contains(name) && !IsZeroOrKeyword(value))
                {
                    violations.Add($"{Local(element)}: {name}=\"{value}\" is a literal metric");
                }
            }
        }

        Assert.Empty(violations);
    }

    private static string LocateTheme()
    {
        var dir = AppContext.BaseDirectory;
        for (var depth = 0; depth < 10 && dir is not null; depth++)
        {
            var candidate = System.IO.Path.Combine(
                dir, "clients", "avalonia", "src", "Spotledger.Client.Controls", "Themes", "Controls.axaml");
            if (System.IO.File.Exists(candidate))
            {
                return candidate;
            }

            dir = System.IO.Path.GetDirectoryName(dir);
        }

        throw new InvalidOperationException("Controls.axaml not found from " + AppContext.BaseDirectory);
    }

    private static bool IsInsideThemeDictionaries(XElement element, XDocument doc) => false; // Controls.axaml has no theme dictionaries

    private static string Local(XElement element) => element.Name.LocalName;

    private static bool LooksLikeColor(string value) =>
        (value.StartsWith('#')
            && (value.Length is 4 or 5 or 7 or 9))
        || NamedColors.Contains(value);

    private static bool IsZeroOrKeyword(string value) =>
        value.Length == 0
        || value is "0"
        || double.TryParse(value, NumberStyles.Float, CultureInfo.InvariantCulture, out var d) && d == 0
        || value.All(c => !char.IsAsciiDigit(c)); // keywords like Auto, *, Left, Center, Stretch
}

/// <summary>
/// RTL: Avalonia mirrors layout, so in a wide SpotField with Urdu labels the label column
/// must sit to the RIGHT of the input on screen, and the decimal editor keeps its digits
/// left-to-right inside the mirrored field.
/// </summary>
public sealed class RtlFieldTests
{
    [AvaloniaFact]
    public void WideFieldInRtlPutsLabelToTheRightOfTheInput()
    {
        var input = new SpotDecimalInput { Scale = 2 };
        var field = new SpotField
        {
            Label = "رقم",
            Content = input,
            IsRequired = true,
            FlowDirection = Avalonia.Media.FlowDirection.RightToLeft,
            WideBreakpoint = 100,
            Width = 600,
        };
        TestHost.Show(field, width: 700);
        Dispatcher.UIThread.RunJobs();

        Assert.True(field.IsWide);
        var label = Find(field, "PART_InlineLabel");
        var content = Find(field, "PART_InlineContent");
        Assert.NotNull(label);
        Assert.NotNull(content);

        // Compare screen coordinates: the label (column 0, mirrored to the right edge)
        // must start right of where the content column ends on the other side.
        var labelLeftScreen = Avalonia.VisualExtensions.PointToScreen(label!, new Point(0, label.Bounds.Height / 2));
        var contentLeftScreen = Avalonia.VisualExtensions.PointToScreen(content!, new Point(0, content.Bounds.Height / 2));
        Assert.True(labelLeftScreen.X > contentLeftScreen.X,
            $"RTL label must be right of the input: label.x={labelLeftScreen.X} content.x={contentLeftScreen.X}");
    }

    [AvaloniaFact]
    public void NarrowFieldStacksLabelAbove()
    {
        var input = new SpotTextInput { Placeholder = "گاہک" };
        var field = new SpotField { Label = "گاہک", Content = input, WideBreakpoint = 100, Width = 80 };
        TestHost.Show(field, width: 100);
        Dispatcher.UIThread.RunJobs();

        Assert.False(field.IsWide);
        var label = Find(field, "PART_StackedLabel");
        Assert.NotNull(label);
    }

    [AvaloniaFact]
    public void DecimalInputInRtlKeepsDigitsLeftToRight()
    {
        var input = new SpotDecimalInput { Scale = 2, Value = 1234.50m, FlowDirection = Avalonia.Media.FlowDirection.RightToLeft };
        TestHost.Show(input);
        Dispatcher.UIThread.RunJobs();

        // The field is mirrored (text aligns to the end), but the value is parsed and
        // displayed LTR: the first character is the most significant digit group.
        Assert.Equal("1,234.50", input.Text);
        Assert.True(HasMirror(input), "the RTL numeric field itself must be mirrored");
        Assert.Equal(1234.50m, input.Value);
    }

    private static bool HasMirror(Avalonia.Visual visual)
    {
        var mirror = typeof(Avalonia.Visual).GetProperty("HasMirrorTransform");
        return mirror is not null && (bool)mirror.GetValue(visual)!;
    }

    private static Control? Find(Control root, string name)
    {
        foreach (var child in Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root))
        {
            if (child is Control c && string.Equals(c.Name, name, StringComparison.Ordinal))
            {
                return c;
            }
        }

        return null;
    }
}
