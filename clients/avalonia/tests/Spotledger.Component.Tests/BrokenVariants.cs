using System.Globalization;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Media;
using Spotledger.Component.Contract;

namespace Spotledger.Component.Tests;

/// <summary>
/// A rating whose theme hard-codes a golden colour instead of using a token. It is otherwise
/// complete, so the only thing the kit may say about it is the token violation.
/// </summary>
internal sealed class BrokenRatingView : ContentControl
{
    protected override AutomationPeer OnCreateAutomationPeer() => new BrokenRatingPeer(this);
}

internal sealed class BrokenRatingPeer : ControlAutomationPeer
{
    public BrokenRatingPeer(BrokenRatingView owner)
        : base(owner)
    {
    }

    protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Slider;
}

/// <summary>A component that is deliberately broken in exactly one way, chosen by its descriptor.</summary>
internal sealed class BrokenRatingComponent : IComponent
{
    private readonly BrokenRatingView _view;
    private readonly TextBlock _stars = new() { Text = "\u2605\u2605\u2605" };
    private readonly bool _announceName;
    private ComponentValue _value;

    public BrokenRatingComponent(ComponentDescriptor descriptor, ComponentContext context, bool announceName)
    {
        ArgumentNullException.ThrowIfNull(context);
        Descriptor = descriptor;
        _announceName = announceName;
        _value = context.Value;
        _view = new BrokenRatingView
        {
            Focusable = true,
            Background = Brushes.Transparent,
            Content = _stars,
        };
        if (announceName)
        {
            AutomationProperties.SetName(_view, context.Field.Label);
        }
    }

    public ComponentDescriptor Descriptor { get; }

    public Control View => _view;

    public ComponentValue Value => _value;

    public ComponentAccessibility Accessibility => new(
        _announceName ? "Rating" : string.Empty,
        ComponentAutomationRole.Slider,
        LiveRegion: ComponentLiveRegion.Polite);

    public KeyboardContract Keyboard => new(
        EntryKeys: new[] { ComponentKey.Tab },
        ExitKeys: new[] { ComponentKey.Tab },
        ActivationKeys: new[] { ComponentKey.Space },
        HandledKeys: new[] { ComponentKey.Tab, ComponentKey.Space, ComponentKey.Enter, ComponentKey.Escape });

    public PerformanceContract Performance => new(BudgetMillisecondsPerUpdate: 8, MaxBytesAllocatedPerUpdate: 8192);

    public event EventHandler<ComponentValidatedEventArgs>? Validated;

    // This test double has no user interaction of its own, so the user-driven events are
    // implemented explicitly: present for the contract, inert by design.
    event EventHandler<ComponentValueChangedEventArgs>? IComponent.ValueChanged
    {
        add { }
        remove { }
    }

    event EventHandler<ComponentCommittedEventArgs>? IComponent.Committed
    {
        add { }
        remove { }
    }

    event EventHandler<ComponentCancelledEventArgs>? IComponent.Cancelled
    {
        add { }
        remove { }
    }

    public IReadOnlyList<ValidationMessage> Validate(ComponentValue proposed) =>
        proposed.IsNull ? new[] { new ValidationMessage("broken.required", "A rating is required.") } : Array.Empty<ValidationMessage>();

    public void SetValue(ComponentValue value)
    {
        _value = value;
        _stars.Text = value.TryGetInteger(out var stars) ? new string('\u2605', (int)Math.Clamp(stars, 0, 5)) : string.Empty;
        Validated?.Invoke(this, new ComponentValidatedEventArgs(Validate(value)));
    }

    public void Dispose()
    {
        // Nothing to release: these test doubles hold no subscription.
    }
}

/// <summary>The two deliberately broken packages and the sound one they are compared with.</summary>
internal static class BrokenRatings
{
    /// <summary>Shares the sample's schema so only the styling differs.</summary>
    public const string SchemaJson = """
        {
          "$schema": "https://json-schema.org/draft/2020-12/schema",
          "type": "object",
          "properties": {
            "max": { "type": "integer", "x-field-type": "int", "minimum": 1, "maximum": 5 }
          },
          "required": ["max"]
        }
        """;

    /// <summary>Hard-codes a colour in its ControlTheme.</summary>
    public static ComponentDescriptor HardCodedColour { get; } = Create(
        "acme.broken.colour",
        "Broken colour rating",
        "avares://Spotledger.Component.Tests/Themes/BrokenColourRating.axaml",
        announceName: true);

    /// <summary>Never sets an accessible name.</summary>
    public static ComponentDescriptor Unnamed { get; } = Create(
        "acme.broken.unnamed",
        "Broken unnamed rating",
        "avares://Spotledger.Component.Tests/Themes/BrokenRating.axaml",
        announceName: false);

    private static ComponentDescriptor Create(string id, string displayName, string themeSource, bool announceName)
    {
        ComponentDescriptor? descriptor = null;
        descriptor = new ComponentDescriptor(
            id,
            displayName,
            new ComponentVersion(1, 0, 0),
            "acme",
            ContractVersionRange.ForMajor(ComponentContract.Version),
            new[] { ComponentFieldType.IntegerNumber },
            SchemaJson,
            new Uri(themeSource),
            context => new BrokenRatingComponent(descriptor!, context, announceName));
        return descriptor;
    }

    /// <summary>Formats a number the way the tests compare it.</summary>
    public static string Invariant(long value) => value.ToString(CultureInfo.InvariantCulture);
}
