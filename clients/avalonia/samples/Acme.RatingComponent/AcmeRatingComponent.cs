using Avalonia.Automation;
using Avalonia.Controls;
using Spotledger.Component.Contract;

namespace Acme.RatingComponent;

/// <summary>
/// The Acme star rating as a contract component: it maps an integer field (1 to 5) onto five
/// stars, validates the range, and declares its accessibility, keyboard and performance
/// behaviour. It reaches the host only through <see cref="ComponentContext"/>: tokens for
/// colours and sizes, focus and localization services, and nothing else.
/// </summary>
public sealed class AcmeRatingComponent : IComponent
{
    /// <summary>The lowest rating the control accepts.</summary>
    public const int MinimumRating = 1;

    /// <summary>The highest rating the control accepts.</summary>
    public const int MaximumRating = AcmeRatingView.StarCount;

    private static readonly ComponentAccessibility AccessibilityInfo = new(
        AutomationName: "Rating",
        Role: ComponentAutomationRole.Slider,
        HelpText: "Rate from one to five stars",
        AutomationId: "acme.rating",
        LiveRegion: ComponentLiveRegion.Assertive);

    private static readonly KeyboardContract KeyboardInfo = new(
        EntryKeys: new[] { ComponentKey.Tab },
        ExitKeys: new[] { ComponentKey.Tab },
        ActivationKeys: new[]
        {
            ComponentKey.ArrowRight,
            ComponentKey.ArrowLeft,
            ComponentKey.ArrowUp,
            ComponentKey.ArrowDown,
            ComponentKey.Home,
            ComponentKey.End,
        },
        HandledKeys: new[]
        {
            ComponentKey.Tab,
            ComponentKey.ArrowRight,
            ComponentKey.ArrowLeft,
            ComponentKey.ArrowUp,
            ComponentKey.ArrowDown,
            ComponentKey.Home,
            ComponentKey.End,
            ComponentKey.Enter,
            ComponentKey.Space,
            ComponentKey.Escape,
        });

    private static readonly PerformanceContract PerformanceInfo = new(
        BudgetMillisecondsPerUpdate: 8,
        MaxBytesAllocatedPerUpdate: 4096);

    private readonly AcmeRatingView _view;
    private ComponentValue _value;
    private string? _lastHelpText;
    private bool _disposed;

    /// <summary>Creates the component for the host's context.</summary>
    public AcmeRatingComponent(ComponentContext context)
    {
        ArgumentNullException.ThrowIfNull(context);
        Context = context;
        _value = context.Value;
        _view = new AcmeRatingView
        {
            Label = context.Field.Label,
            IsEditable = !context.IsReadOnly,
            IsEnabled = !context.IsDisabled,
            Focusable = true,
            Value = RatingOf(_value),
        };
        AutomationProperties.SetName(_view, context.Field.Label);
        AutomationProperties.SetAutomationId(_view, "acme.rating." + context.Field.Name);
        AutomationProperties.SetLiveSetting(_view, AutomationLiveSetting.Assertive);
        _view.ValueEditRequested += OnValueEditRequested;
        _view.CommitRequested += OnCommitRequested;
        _view.CancelRequested += OnCancelRequested;
        UpdateHelpText(Validate(_value));
    }

    /// <inheritdoc />
    public ComponentDescriptor Descriptor => AcmeRatingDescriptor.Instance;

    /// <inheritdoc />
    public Control View => _view;

    /// <inheritdoc />
    public ComponentValue Value => _value;

    /// <summary>The context the host created this instance with.</summary>
    public ComponentContext Context { get; }

    /// <inheritdoc />
    public ComponentAccessibility Accessibility => AccessibilityInfo with { AutomationName = Context.Field.Label };

    /// <inheritdoc />
    public KeyboardContract Keyboard => KeyboardInfo;

    /// <inheritdoc />
    public PerformanceContract Performance => PerformanceInfo;

    /// <inheritdoc />
    public event EventHandler<ComponentValueChangedEventArgs>? ValueChanged;

    /// <inheritdoc />
    public event EventHandler<ComponentCommittedEventArgs>? Committed;

    /// <inheritdoc />
    public event EventHandler<ComponentCancelledEventArgs>? Cancelled;

    /// <inheritdoc />
    public event EventHandler<ComponentValidatedEventArgs>? Validated;

    /// <summary>
    /// The validation hook: a rating must be present and between one and five. Pure — no clock,
    /// no services, no state.
    /// </summary>
    public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed)
    {
        if (proposed.Type != ComponentFieldType.IntegerNumber)
        {
            return new[]
            {
                new ValidationMessage("acme.rating.type", "A rating binds to a whole number."),
            };
        }

        if (proposed.IsNull || proposed.IsEmpty || !proposed.TryGetInteger(out var stars))
        {
            return Context.Field.Required
                ? new[] { new ValidationMessage("acme.rating.required", "A rating is required.") }
                : Array.Empty<ValidationMessage>();
        }

        if (stars < MinimumRating || stars > MaximumRating)
        {
            return new[]
            {
                new ValidationMessage(
                    "acme.rating.range",
                    "A rating is between " + MinimumRating.ToString(System.Globalization.CultureInfo.InvariantCulture)
                    + " and " + MaximumRating.ToString(System.Globalization.CultureInfo.InvariantCulture) + " stars."),
            };
        }

        return Array.Empty<ValidationMessage>();
    }

    /// <inheritdoc />
    public void SetValue(ComponentValue value)
    {
        if (value.Type != ComponentFieldType.IntegerNumber)
        {
            throw new ArgumentException("A rating takes an integer value, not " + value.Type.ToWireName() + ".", nameof(value));
        }

        _value = value;
        _view.Value = RatingOf(value);
        var messages = Validate(value);
        UpdateHelpText(messages);
        Validated?.Invoke(this, new ComponentValidatedEventArgs(messages));
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _view.ValueEditRequested -= OnValueEditRequested;
        _view.CommitRequested -= OnCommitRequested;
        _view.CancelRequested -= OnCancelRequested;
    }

    private static int RatingOf(ComponentValue value) =>
        value.TryGetInteger(out var stars) ? (int)Math.Clamp(stars, 0, MaximumRating) : 0;

    private void OnValueEditRequested(object? sender, int stars)
    {
        var previous = _value;
        var next = ComponentValue.FromInteger(stars);
        _value = next;
        _view.Value = stars;
        var messages = Validate(next);
        UpdateHelpText(messages);
        ValueChanged?.Invoke(this, new ComponentValueChangedEventArgs(next, previous));
        Validated?.Invoke(this, new ComponentValidatedEventArgs(messages));
    }

    private void OnCommitRequested(object? sender, EventArgs args) =>
        Committed?.Invoke(this, new ComponentCommittedEventArgs(_value));

    private void OnCancelRequested(object? sender, EventArgs args) =>
        Cancelled?.Invoke(this, new ComponentCancelledEventArgs(_value));

    /// <summary>
    /// Mirrors the validation messages into the accessibility tree. Only when the text actually
    /// changes, so a value update does not allocate on every keystroke.
    /// </summary>
    private void UpdateHelpText(IReadOnlyList<ValidationMessage> messages)
    {
        var text = messages.Count == 0
            ? AccessibilityInfo.HelpText
            : string.Join(" ", messages.Select(static message => message.Message));
        if (string.Equals(text, _lastHelpText, StringComparison.Ordinal))
        {
            return;
        }

        _lastHelpText = text;
        AutomationProperties.SetHelpText(_view, text);
    }
}
