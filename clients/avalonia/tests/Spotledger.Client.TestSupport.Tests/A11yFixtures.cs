using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Automation.Provider;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.VisualTree;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// Deliberately broken and deliberately correct fixtures, one pair per audit rule. The pair is the
/// proof that a rule reports the defect it claims to report and stays quiet on a good control.
/// </summary>
internal static class AccessibilityFixtures
{
    /// <summary>A form field with an explicit accessible name (passing) or none (failing).</summary>
    public static Control TextBox(bool named)
    {
        var box = new TextBox { PlaceholderText = "Amount" };
        if (named) AutomationProperties.SetName(box, "Amount");
        return new StackPanel { Children = { new TextBlock { Text = "Amount" }, box } };
    }

    /// <summary>A labelled field where the label is linked with LabeledBy instead of a name.</summary>
    public static Control LabeledTextBox(bool linked)
    {
        var label = new TextBlock { Text = "Customer" };
        var box = new TextBox();
        if (linked) AutomationProperties.SetLabeledBy(box, label);
        return new StackPanel { Children = { label, box } };
    }

    /// <summary>An icon-only button: named (passing) or nameless with a graphic content (failing).</summary>
    public static Control IconButton(bool named)
    {
        var button = new Button
        {
            Content = new Border { Width = 12, Height = 12, Background = Brushes.DimGray, CornerRadius = new CornerRadius(2) },
            Width = 32,
            Height = 32,
        };
        if (named) AutomationProperties.SetName(button, "Search");
        return new StackPanel { Orientation = Orientation.Horizontal, Children = { button, new TextBlock { Text = "Search" } } };
    }

    /// <summary>Two buttons whose names collide (failing) or are distinct (passing).</summary>
    public static Control Buttons(bool distinct) => new StackPanel
    {
        Orientation = Orientation.Horizontal,
        Children =
        {
            new Button { Content = "Save" },
            new Button { Content = distinct ? "Cancel" : "Save" },
        },
    };

    /// <summary>
    /// A grid-shaped control with no automation peer at all (failing) and one that exposes rows and
    /// a selection pattern (passing).
    /// </summary>
    public static Control Grid(bool accessible) => accessible ? new AccessibleTestGrid() : new TestGrid();

    /// <summary>Tab order contradicts reading order when <paramref name="inverted"/> (failing).</summary>
    public static Control TabOrder(bool inverted, FlowDirection direction = FlowDirection.LeftToRight) => new StackPanel
    {
        Orientation = Orientation.Horizontal,
        FlowDirection = direction,
        Children =
        {
            new Button { Content = "First", TabIndex = inverted ? 1 : 0, Width = 100 },
            new Button { Content = "Second", TabIndex = inverted ? 0 : 1, Width = 100 },
        },
    };

    /// <summary>A focusable box that paints a focus border (passing) or nothing (failing).</summary>
    public static Control FocusRing(bool indicates) => new StackPanel
    {
        Children =
        {
            new Border { Height = 8 },
            new FocusFixture { Indicates = indicates },
        },
    };

    /// <summary>Text at a contrast ratio the rule must reject (failing) or accept (passing).</summary>
    public static Control Contrast(bool passing, bool large = false) => new Border
    {
        Background = Brushes.White,
        Padding = new Thickness(4),
        Child = new TextBlock
        {
            Text = "Contrast sample text",
            FontSize = large ? 28 : 16,
            FontWeight = FontWeight.Normal,
            Foreground = passing ? Brushes.Black : new SolidColorBrush(Color.Parse("#BBBBBB")),
        },
    };

    /// <summary>A bright grey on white that is fine as large text (3:1) but not as ordinary text.</summary>
    public static Control LargeTextContrast() => new Border
    {
        Background = Brushes.White,
        Padding = new Thickness(4),
        Child = new TextBlock { Text = "Large heading", FontSize = 28, Foreground = new SolidColorBrush(Color.Parse("#767676")) },
    };
}

/// <summary>A focusable box that shows a 2 px focus border only when <see cref="Indicates"/> is set.</summary>
internal sealed class FocusFixture : Border
{
    private static readonly IBrush Ring = new SolidColorBrush(Color.Parse("#D00000"));

    public FocusFixture()
    {
        Width = 120;
        Height = 40;
        Background = Brushes.WhiteSmoke;
        BorderThickness = new Thickness(2);
        BorderBrush = Brushes.Transparent;
        Focusable = true;
        IsTabStop = true;
        // No theme-drawn focus adorner: only 'Indicates' may paint an indicator, so the failing
        // fixture is genuinely invisible and the passing one proves the rule detects the border.
        FocusAdorner = null;
    }

    public bool Indicates { get; init; }

    protected override void OnGotFocus(FocusChangedEventArgs e)
    {
        base.OnGotFocus(e);
        if (Indicates) BorderBrush = Ring;
    }

    protected override void OnLostFocus(FocusChangedEventArgs e)
    {
        base.OnLostFocus(e);
        BorderBrush = Brushes.Transparent;
    }
}

/// <summary>A grid-shaped control with no automation peer: what a screen reader sees as a blank box.</summary>
internal sealed class TestGrid : Panel
{
    public TestGrid()
    {
        Width = 120;
        Height = 60;
        Background = Brushes.WhiteSmoke;
        Children.Add(new TextBlock { Text = "row 1" });
    }
}

/// <summary>A grid-shaped control that does expose rows and a selection pattern through its peer.</summary>
internal sealed class AccessibleTestGrid : Panel
{
    public AccessibleTestGrid()
    {
        Width = 120;
        Height = 60;
        Background = Brushes.WhiteSmoke;
        for (var index = 0; index < 2; index++)
        {
            var row = new ListBoxItem { Content = "row " + (index + 1) };
            AutomationProperties.SetName(row, "row " + (index + 1));
            Children.Add(row);
        }
    }

    protected override AutomationPeer OnCreateAutomationPeer() => new AccessibleTestGridPeer(this);
}

internal sealed class AccessibleTestGridPeer(Control owner) : ControlAutomationPeer(owner), ISelectionProvider
{
    private readonly Control _owner = owner;

    public bool CanSelectMultiple => false;

    public bool IsSelectionRequired => true;

    public IReadOnlyList<AutomationPeer> GetSelection() => RowPeers().Take(1).ToList();

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.DataGrid;

    protected override string GetNameCore() => "Test grid";

    protected override IReadOnlyList<AutomationPeer>? GetChildrenCore() => RowPeers();

    private List<AutomationPeer> RowPeers()
    {
        var peers = new List<AutomationPeer>();
        foreach (var child in _owner.GetVisualChildren().OfType<Control>())
            if (ControlAutomationPeer.CreatePeerForElement(child) is { } peer) peers.Add(peer);
        return peers;
    }
}
