using Avalonia.Input;
using Avalonia.Media;

namespace Spotledger.Component.Contract;

/// <summary>
/// Read-only access to the design system's tokens. A component reads every colour, size, font
/// and radius through this interface (or through <c>DynamicResource</c> in its ControlTheme);
/// it may never hard-code a literal, which is what keeps an app's screen native in both themes.
/// There is no setter: a component cannot change the theme.
/// </summary>
public interface ITokenLookup
{
    /// <summary>Every token key this host resolves.</summary>
    IReadOnlyCollection<string> Keys { get; }

    /// <summary>Looks a token up, returning false when the key is unknown to this host.</summary>
    bool TryGet(string key, out object? value);

    /// <summary>The token as a colour; null when absent or not a colour.</summary>
    Color? GetColor(string key);

    /// <summary>The token as a length; <paramref name="fallback"/> when absent or not a length.</summary>
    double GetLength(string key, double fallback);
}

/// <summary>
/// The services a component may use, and nothing else: focus, clipboard text and localization.
/// There is deliberately no network client, no file system, no reflection and no ambient clock.
/// </summary>
/// <param name="Focus">Moves focus within the component's own visual subtree.</param>
/// <param name="Clipboard">Plain-text clipboard access, granted or withheld by the host.</param>
/// <param name="Localizer">Translates keys the host owns.</param>
public sealed record ComponentServices(IFocusService Focus, IClipboardService Clipboard, ILocalizer Localizer)
{
    /// <summary>The service set a headless test host passes when it grants nothing.</summary>
    public static ComponentServices Empty { get; } = new(
        new NoFocusService(),
        new NoClipboardService(),
        new PassThroughLocalizer());
}

/// <summary>Focus movement, limited to elements the component already owns.</summary>
public interface IFocusService
{
    /// <summary>Focuses <paramref name="target"/>, or does nothing when the host refuses.</summary>
    void Focus(IInputElement target);
}

/// <summary>Plain-text clipboard access.</summary>
public interface IClipboardService
{
    /// <summary>The clipboard's text, or null when empty or refused.</summary>
    Task<string?> GetTextAsync();

    /// <summary>Replaces the clipboard's text.</summary>
    Task SetTextAsync(string text);
}

/// <summary>Translation of host-owned keys.</summary>
public interface ILocalizer
{
    /// <summary>The translation of <paramref name="key"/>, or the key itself when unknown.</summary>
    string Translate(string key);

    /// <summary>The translation of <paramref name="key"/> with its arguments substituted.</summary>
    string Format(string key, params object?[] arguments);
}

/// <summary>The locale and direction the host renders in.</summary>
/// <param name="Culture">The BCP-47 culture name, for example <c>ur-PK</c>.</param>
/// <param name="IsRightToLeft">True when the layout must mirror.</param>
public sealed record LocaleInfo(string Culture, bool IsRightToLeft)
{
    /// <summary>English, left to right: the default the kit and the sample use.</summary>
    public static LocaleInfo InvariantEnglish { get; } = new("en-US", false);

    /// <summary>Urdu, right to left: the smoke locale for mirroring.</summary>
    public static LocaleInfo Urdu { get; } = new("ur-PK", true);
}

/// <summary>The two density modes the design system renders.</summary>
public enum ComponentDensity
{
    /// <summary>Roomier rows and padding.</summary>
    Comfortable,

    /// <summary>The compact mode a power user picks.</summary>
    Compact,
}

/// <summary>
/// Everything a component is told when it is created, and nothing more: the value, the field,
/// whether it is editable, the locale and direction, the density, the tokens and the allowed
/// services. There is no host object behind this, so a component cannot reach the app.
/// </summary>
public sealed class ComponentContext
{
    /// <summary>Creates a context.</summary>
    public ComponentContext(
        ComponentValue value,
        FieldMetadata field,
        bool isReadOnly = false,
        bool isDisabled = false,
        LocaleInfo? locale = null,
        ComponentDensity density = ComponentDensity.Comfortable,
        ITokenLookup? tokens = null,
        ComponentServices? services = null)
    {
        ArgumentNullException.ThrowIfNull(field);
        if (value.Type != field.Type)
        {
            throw new ArgumentException(
                "The value's type " + value.Type + " does not match the field's type " + field.Type + ".",
                nameof(value));
        }

        Value = value;
        Field = field;
        IsReadOnly = isReadOnly || field.ReadOnly;
        IsDisabled = isDisabled;
        Locale = locale ?? LocaleInfo.InvariantEnglish;
        Density = density;
        Tokens = tokens ?? EmptyTokenLookup.Instance;
        Services = services ?? ComponentServices.Empty;
    }

    /// <summary>The initial value.</summary>
    public ComponentValue Value { get; }

    /// <summary>The field being edited.</summary>
    public FieldMetadata Field { get; }

    /// <summary>True when the value is shown but not editable.</summary>
    public bool IsReadOnly { get; }

    /// <summary>True when the component is disabled entirely.</summary>
    public bool IsDisabled { get; }

    /// <summary>The locale and direction.</summary>
    public LocaleInfo Locale { get; }

    /// <summary>The density mode.</summary>
    public ComponentDensity Density { get; }

    /// <summary>The token lookup.</summary>
    public ITokenLookup Tokens { get; }

    /// <summary>The services granted to this instance.</summary>
    public ComponentServices Services { get; }

    /// <summary>A copy of this context with another value: what a host does on every change.</summary>
    public ComponentContext WithValue(ComponentValue value) =>
        new(value, Field, IsReadOnly, IsDisabled, Locale, Density, Tokens, Services);
}

/// <summary>The lookup a context gets when the host supplies none: every key is unknown.</summary>
public sealed class EmptyTokenLookup : ITokenLookup
{
    /// <summary>The single instance.</summary>
    public static EmptyTokenLookup Instance { get; } = new();

    /// <inheritdoc />
    public IReadOnlyCollection<string> Keys => Array.Empty<string>();

    /// <inheritdoc />
    public bool TryGet(string key, out object? value)
    {
        value = null;
        return false;
    }

    /// <inheritdoc />
    public Color? GetColor(string key) => null;

    /// <inheritdoc />
    public double GetLength(string key, double fallback) => fallback;
}

internal sealed class NoFocusService : IFocusService
{
    public void Focus(IInputElement target)
    {
        // Deliberately inert: a host that grants no focus service.
    }
}

internal sealed class NoClipboardService : IClipboardService
{
    public Task<string?> GetTextAsync() => Task.FromResult<string?>(null);

    public Task SetTextAsync(string text) => Task.CompletedTask;
}

internal sealed class PassThroughLocalizer : ILocalizer
{
    public string Translate(string key) => key;

    public string Format(string key, params object?[] arguments) =>
        arguments is null || arguments.Length == 0 ? key : key + "(" + string.Join(",", arguments) + ")";
}
