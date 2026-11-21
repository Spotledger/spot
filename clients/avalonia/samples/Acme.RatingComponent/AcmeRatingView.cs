using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Input;
using Avalonia.Media;

namespace Acme.RatingComponent;

/// <summary>
/// The rating control's view: a label and five stars, all styled from Theme tokens by
/// <c>Themes/AcmeRating.axaml</c>. It owns no state beyond the numbers it shows; the component
/// above it owns the value, validation and events.
/// </summary>
public class AcmeRatingView : TemplatedControl
{
    /// <summary>The number of stars drawn.</summary>
    public const int StarCount = 5;

    /// <summary>The current rating, 0 to <see cref="StarCount"/>.</summary>
    public static readonly StyledProperty<int> ValueProperty =
        AvaloniaProperty.Register<AcmeRatingView, int>(nameof(Value));

    /// <summary>The field label shown above the stars.</summary>
    public static readonly StyledProperty<string> LabelProperty =
        AvaloniaProperty.Register<AcmeRatingView, string>(nameof(Label), string.Empty);

    /// <summary>False when the host binds the field read-only: stars are shown, clicks are ignored.</summary>
    public static readonly StyledProperty<bool> IsEditableProperty =
        AvaloniaProperty.Register<AcmeRatingView, bool>(nameof(IsEditable), true);

    private static readonly string FilledStar = "\u2605";
    private static readonly string EmptyStar = "\u2606";

    private readonly List<TextBlock> _stars = new();
    private StackPanel? _starPanel;

    /// <summary>Raised when the user picks a rating; the argument is the chosen star count.</summary>
    public event EventHandler<int>? ValueEditRequested;

    /// <summary>Raised when the user presses Enter.</summary>
    public event EventHandler<EventArgs>? CommitRequested;

    /// <summary>Raised when the user presses Escape.</summary>
    public event EventHandler<EventArgs>? CancelRequested;

    /// <summary>The current rating.</summary>
    public int Value
    {
        get => GetValue(ValueProperty);
        set => SetValue(ValueProperty, value);
    }

    /// <summary>The field label.</summary>
    public string Label
    {
        get => GetValue(LabelProperty);
        set => SetValue(LabelProperty, value);
    }

    /// <summary>Whether the user may change the rating.</summary>
    public bool IsEditable
    {
        get => GetValue(IsEditableProperty);
        set => SetValue(IsEditableProperty, value);
    }

    /// <inheritdoc />
    protected override void OnApplyTemplate(TemplateAppliedEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnApplyTemplate(e);
        _starPanel = e.NameScope.Find<StackPanel>("PART_Stars");
        _stars.Clear();
        if (_starPanel is null)
        {
            return;
        }

        _starPanel.Children.Clear();
        for (var index = 0; index < StarCount; index++)
        {
            var star = CreateStar(index);
            _stars.Add(star);
            _starPanel.Children.Add(star);
        }

        RefreshStars();
    }

    /// <inheritdoc />
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        ArgumentNullException.ThrowIfNull(change);
        base.OnPropertyChanged(change);
        if (change.Property == ValueProperty || change.Property == ForegroundProperty || change.Property == FontSizeProperty)
        {
            RefreshStars();
        }

        if (change.Property == IsEditableProperty)
        {
            PseudoClasses.Set(":editable", IsEditable);
        }
    }

    /// <inheritdoc />
    protected override void OnKeyDown(KeyEventArgs e)
    {
        ArgumentNullException.ThrowIfNull(e);
        base.OnKeyDown(e);
        if (e.Handled)
        {
            return;
        }

        switch (e.Key)
        {
            case Key.Right:
            case Key.Up:
                e.Handled = true;
                if (IsEditable)
                {
                    RequestEdit(Math.Min(StarCount, Value + 1));
                }

                break;
            case Key.Left:
            case Key.Down:
                e.Handled = true;
                if (IsEditable)
                {
                    RequestEdit(Math.Max(1, Value - 1));
                }

                break;
            case Key.Home:
                e.Handled = true;
                if (IsEditable)
                {
                    RequestEdit(1);
                }

                break;
            case Key.End:
                e.Handled = true;
                if (IsEditable)
                {
                    RequestEdit(StarCount);
                }

                break;
            case Key.Enter:
            case Key.Space:
                e.Handled = true;
                CommitRequested?.Invoke(this, EventArgs.Empty);
                break;
            case Key.Escape:
                e.Handled = true;
                CancelRequested?.Invoke(this, EventArgs.Empty);
                break;
            default:
                break;
        }
    }

    /// <inheritdoc />
    protected override AutomationPeer OnCreateAutomationPeer() => new AcmeRatingAutomationPeer(this);

    private TextBlock CreateStar(int index)
    {
        var star = new TextBlock
        {
            Name = "PART_Star" + index.ToString(System.Globalization.CultureInfo.InvariantCulture),
            Text = EmptyStar,
            Focusable = false,
            VerticalAlignment = Avalonia.Layout.VerticalAlignment.Center,
            FontFamily = FontFamily,
            FontSize = FontSize,
            Foreground = Foreground,
        };
        AutomationProperties.SetName(star, (index + 1).ToString(System.Globalization.CultureInfo.InvariantCulture) + " of " + StarCount);
        star.PointerPressed += (_, args) =>
        {
            args.Handled = true;
            if (IsEditable)
            {
                RequestEdit(index + 1);
            }
        };
        return star;
    }

    private void RefreshStars()
    {
        for (var index = 0; index < _stars.Count; index++)
        {
            var star = _stars[index];
            star.Text = index < Value ? FilledStar : EmptyStar;
            star.FontSize = FontSize;
            star.FontFamily = FontFamily;
            star.Foreground = Foreground;
            star.Opacity = index < Value ? 1 : 0.45;
        }
    }

    private void RequestEdit(int stars) => ValueEditRequested?.Invoke(this, stars);
}

/// <summary>The peer that announces the control as a slider, which is what a rating is.</summary>
internal sealed class AcmeRatingAutomationPeer : ControlAutomationPeer
{
    public AcmeRatingAutomationPeer(AcmeRatingView owner)
        : base(owner)
    {
    }

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Slider;
}
