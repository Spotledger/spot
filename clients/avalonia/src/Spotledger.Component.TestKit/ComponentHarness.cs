using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Markup.Xaml.Styling;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Theme;
using Spotledger.Component.Contract;

namespace Spotledger.Component.TestKit;

/// <summary>
/// The token lookup a headless host passes to a component: it resolves keys from the application's
/// resources (Theme tokens merged by the test app), read-only, exactly as the shell does.
/// </summary>
public sealed class HostTokenLookup : ITokenLookup
{
    private readonly Application _application;

    /// <summary>Creates the lookup over the running headless application.</summary>
    public HostTokenLookup(Application application)
    {
        _application = application ?? throw new ArgumentNullException(nameof(application));
    }

    /// <inheritdoc />
    public IReadOnlyCollection<string> Keys
    {
        get
        {
            var keys = new HashSet<string>(StringComparer.Ordinal);
            Collect(_application.Resources, keys);
            foreach (var tokenField in typeof(Tokens).GetFields(System.Reflection.BindingFlags.Public | System.Reflection.BindingFlags.Static))
            {
                if (tokenField.IsLiteral && tokenField.GetRawConstantValue() is string token)
                {
                    keys.Add(token);
                }
            }

            return keys;
        }
    }

    /// <inheritdoc />
    public bool TryGet(string key, out object? value)
    {
        value = null;
        return key is not null && _application.TryGetResource(key, ThemeVariant.Light, out value);
    }

    /// <inheritdoc />
    public Color? GetColor(string key)
    {
        if (!TryGet(key, out var value))
        {
            return null;
        }

        return value switch
        {
            Color color => color,
            SolidColorBrush brush => brush.Color,
            _ => null,
        };
    }

    /// <inheritdoc />
    public double GetLength(string key, double fallback) =>
        TryGet(key, out var value) && value is double length ? length : fallback;

    private static void Collect(IResourceProvider resources, HashSet<string> keys)
    {
        if (resources is not IResourceDictionary dictionary)
        {
            return;
        }

        foreach (var key in dictionary.Keys)
        {
            if (key is string name)
            {
                keys.Add(name);
            }
        }

        foreach (var merged in dictionary.MergedDictionaries)
        {
            Collect(merged, keys);
        }

        if (dictionary is ResourceDictionary resourceDictionary)
        {
            foreach (var variant in resourceDictionary.ThemeDictionaries.Values)
            {
                Collect(variant, keys);
            }
        }
    }
}

/// <summary>A focus service that focuses for real, and a localizer that translates two known keys.</summary>
internal sealed class TestFocusService : IFocusService
{
    public void Focus(IInputElement target) => target?.Focus();
}

internal sealed class TestClipboardService : IClipboardService
{
    private string? _text;

    public Task<string?> GetTextAsync() => Task.FromResult(_text);

    public Task SetTextAsync(string text)
    {
        _text = text;
        return Task.CompletedTask;
    }
}

internal sealed class TestLocalizer : ILocalizer
{
    private readonly bool _urdu;

    public TestLocalizer(bool urdu) => _urdu = urdu;

    public string Translate(string key) => _urdu ? UrduSamples.InvalidMessage : key;

    public string Format(string key, params object?[] arguments) =>
        arguments is null || arguments.Length == 0 ? Translate(key) : Translate(key) + " " + string.Join(" ", arguments);
}

/// <summary>Builds the contexts and contexts-with-variants the kit drives a component through.</summary>
public static class TestContexts
{
    /// <summary>The tokens of the running headless application.</summary>
    public static ITokenLookup Tokens =>
        Application.Current is { } app ? new HostTokenLookup(app) : EmptyTokenLookup.Instance;

    /// <summary>A context for <paramref name="type"/> with the kit's defaults.</summary>
    public static ComponentContext Create(
        ComponentDescriptor descriptor,
        ComponentFieldType type,
        bool required = true,
        bool readOnly = false,
        bool disabled = false,
        LocaleInfo? locale = null,
        ComponentDensity density = ComponentDensity.Comfortable,
        ComponentValue? value = null)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        var effectiveLocale = locale ?? LocaleInfo.InvariantEnglish;
        var label = effectiveLocale.IsRightToLeft ? UrduSamples.RatingLabel : "Rating";
        var field = new FieldMetadata("rating", label, type, required, readOnly, HelpText: "Rate from one to five");
        return new ComponentContext(
            value ?? SampleValues.Canonical(type),
            field,
            readOnly,
            disabled,
            effectiveLocale,
            density,
            Tokens,
            new ComponentServices(new TestFocusService(), new TestClipboardService(), new TestLocalizer(effectiveLocale.IsRightToLeft)));
    }
}

/// <summary>
/// Shows a component in a headless window, merges its ControlTheme, and offers the input and
/// layout helpers a contract check needs.
/// </summary>
public sealed class ComponentHarness : IDisposable
{
    private readonly Panel _host;
    private bool _disposed;

    private ComponentHarness(IComponent component, Window window, Panel host)
    {
        Component = component;
        Window = window;
        _host = host;
    }

    /// <summary>The component under test.</summary>
    public IComponent Component { get; }

    /// <summary>The window the component is shown in.</summary>
    public Window Window { get; }

    /// <summary>Merges the descriptor's ControlTheme and shows an instance created from <paramref name="context"/>.</summary>
    public static ComponentHarness Show(
        ComponentDescriptor descriptor,
        ComponentContext context,
        double width = 600,
        double height = 200,
        FlowDirection direction = FlowDirection.LeftToRight)
    {
        ArgumentNullException.ThrowIfNull(descriptor);
        MergeTheme(descriptor.ThemeSource);
        var component = descriptor.Create(context);
        return ShowComponent(component, width, height, direction);
    }

    /// <summary>Shows an already-created component (used by the broken-variant tests).</summary>
    public static ComponentHarness ShowComponent(
        IComponent component,
        double width = 600,
        double height = 200,
        FlowDirection direction = FlowDirection.LeftToRight)
    {
        ArgumentNullException.ThrowIfNull(component);
        var host = new Panel { HorizontalAlignment = Avalonia.Layout.HorizontalAlignment.Left };
        host.Children.Add(component.View);
        var window = new Window
        {
            Width = width,
            Height = height,
            FlowDirection = direction,
            Content = host,
        };
        window.Show();
        Dispatcher.UIThread.RunJobs();
        return new ComponentHarness(component, window, host);
    }

    /// <summary>Merges <paramref name="themeSource"/> into the application's resources once.</summary>
    public static void MergeTheme(Uri themeSource)
    {
        ArgumentNullException.ThrowIfNull(themeSource);
        if (Application.Current is not { } app)
        {
            throw new InvalidOperationException("A headless Avalonia application must be running: the test assembly needs [assembly: AvaloniaTestApplication(...)].");
        }

        var alreadyMerged = app.Resources.MergedDictionaries
            .OfType<ResourceInclude>()
            .Any(include => include.Source is not null && include.Source.Equals(themeSource));
        if (alreadyMerged)
        {
            return;
        }

        var baseUri = new Uri(themeSource.GetLeftPart(UriPartial.Authority) + "/");
        app.Resources.MergedDictionaries.Add(new ResourceInclude(baseUri) { Source = themeSource });
        Dispatcher.UIThread.RunJobs();
    }

    /// <summary>Runs the dispatcher queue until idle: layout, template application, bindings.</summary>
    public static void Pump() => Dispatcher.UIThread.RunJobs();

    /// <summary>Sends one key press and release to the window's focused element.</summary>
    public void SendKey(Key key, RawInputModifiers modifiers = RawInputModifiers.None)
    {
        Window.KeyPress(key, modifiers, PhysicalKey.None, null);
        Window.KeyRelease(key, modifiers, PhysicalKey.None, null);
        Pump();
    }

    /// <summary>Sends a contract key.</summary>
    public void SendKey(ComponentKey key) => SendKey(ToAvaloniaKey(key));

    /// <summary>Maps a contract key to the toolkit's key.</summary>
    public static Key ToAvaloniaKey(ComponentKey key) => key switch
    {
        ComponentKey.Tab => Key.Tab,
        ComponentKey.Enter => Key.Enter,
        ComponentKey.Space => Key.Space,
        ComponentKey.Escape => Key.Escape,
        ComponentKey.ArrowLeft => Key.Left,
        ComponentKey.ArrowRight => Key.Right,
        ComponentKey.ArrowUp => Key.Up,
        ComponentKey.ArrowDown => Key.Down,
        ComponentKey.Home => Key.Home,
        ComponentKey.End => Key.End,
        ComponentKey.PageUp => Key.PageUp,
        ComponentKey.PageDown => Key.PageDown,
        ComponentKey.Delete => Key.Delete,
        ComponentKey.Backspace => Key.Back,
        ComponentKey.F2 => Key.F2,
        _ => throw new ArgumentOutOfRangeException(nameof(key), key, "Unknown contract key."),
    };

    /// <summary>The element that currently has focus, or null.</summary>
    public IInputElement? FocusedElement =>
        TopLevel.GetTopLevel(Component.View)?.FocusManager?.GetFocusedElement();

    /// <summary>Focuses the component's view.</summary>
    public void FocusView()
    {
        Component.View.Focus();
        Pump();
    }

    /// <summary>Adds a sibling after the component, so a tab-out has somewhere to go.</summary>
    public Button AddTrailingButton()
    {
        var button = new Button { Content = "after", Name = "AfterButton" };
        _host.Children.Add(button);
        Pump();
        return button;
    }

    /// <summary>Every named, visible descendant of the view, in tree order.</summary>
    public IReadOnlyList<Control> NamedDescendants() =>
        Component.View.GetVisualDescendants().OfType<Control>().Where(c => !string.IsNullOrEmpty(c.Name)).ToList();

    /// <summary>The absolute position of <paramref name="control"/> in the window.</summary>
    public Point AbsolutePosition(Control control)
    {
        ArgumentNullException.ThrowIfNull(control);
        var point = control.TranslatePoint(new Point(0, 0), Window);
        return point ?? new Point(double.NaN, double.NaN);
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        try
        {
            _host.Children.Remove(Component.View);
            Window.Content = null;
            Window.Close();
            Pump();
        }
        finally
        {
            Component.Dispose();
        }
    }
}
