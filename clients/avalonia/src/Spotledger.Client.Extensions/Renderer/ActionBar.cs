using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Extensions;
using TextBlock = Avalonia.Controls.TextBlock;

namespace Spotledger.Client.Extensions.Renderer;

/// <summary>
/// The action surface of a slot that accepts actions (<c>doc.toolbar</c>,
/// <c>list.toolbar</c>, the shell's bar slots): one catalog button per visible action, in
/// the slot's render order. Each button takes its tab stop from that order (the documented
/// tab order), its accessible name from the label, its busy state from the model, and it
/// invokes the model exactly once per click. Text wraps, so an Urdu label and a long English
/// label both fit at the narrowest supported width.
/// </summary>
public sealed class ActionBar : UserControl
{
    private readonly WrapPanel _buttons = new() { Orientation = Orientation.Horizontal };
    private double _gap;
    private SlotView? _view;
    private readonly Dictionary<string, (SpotButton Button, ActionModel Model)> _byId = new(StringComparer.Ordinal);

    /// <summary>Creates the bar.</summary>
    public ActionBar()
    {
        Content = _buttons;
        HorizontalAlignment = HorizontalAlignment.Stretch;
        Loaded += (_, _) => ApplyTheme();
        ActualThemeVariantChanged += (_, _) => ApplyTheme();
    }

    /// <summary>The view this bar draws, or null.</summary>
    public SlotView? View => _view;

    /// <summary>The accessible name of each drawn action, in tab order (tests).</summary>
    public IReadOnlyList<string> AccessibleNames =>
        [.. _buttons.Children.OfType<SpotButton>().Select(button => AutomationProperties.GetName(button) ?? string.Empty)];

    /// <summary>Shows one slot view; subscribes to its changes so visibility and busy state follow the model.</summary>
    public void Show(SlotView view)
    {
        ArgumentNullException.ThrowIfNull(view);
        if (_view is not null)
        {
            _view.Changed -= OnViewChanged;
        }
        _view = view;
        _view.Changed += OnViewChanged;
        Rebuild();
    }

    /// <summary>Rebuilds the buttons after a theme or density change.</summary>
    public void ApplyTheme() => Rebuild();


    /// <inheritdoc/>
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == Density.ModeProperty)
        {
            Rebuild();
        }
    }

    private void OnViewChanged(object? sender, EventArgs e)
    {
        // Entries change identity only when the visible set changes; otherwise update in place
        // so a click's focus and the busy state are not thrown away.
        if (_view is null)
        {
            return;
        }
        IReadOnlyList<SlotEntry> entries = _view.Entries;
        bool sameSet = entries.Count == _byId.Count &&
            entries.All(entry => _byId.TryGetValue(entry.Id, out (SpotButton Button, ActionModel Model) pair) && ReferenceEquals(pair.Model, entry));
        if (sameSet)
        {
            foreach (SlotEntry entry in entries)
            {
                if (entry is ActionModel action && _byId.TryGetValue(entry.Id, out (SpotButton Button, ActionModel Model) pair))
                {
                    pair.Button.IsEnabled = action.IsEnabled;
                    pair.Button.IsBusy = action.IsBusy;
                }
            }
            return;
        }
        Rebuild();
    }

    private void Rebuild()
    {
        _buttons.Children.Clear();
        _byId.Clear();
        _gap = ThemeAccess.FieldGap(this);
        if (_view is null)
        {
            return;
        }
        int tabIndex = 0;
        foreach (SlotEntry entry in _view.Entries)
        {
            if (entry is not ActionModel action)
            {
                continue;
            }
            SpotButton button = new()
            {
                Variant = SpotButtonVariant.Primary,
                IsEnabled = action.IsEnabled,
                IsBusy = action.IsBusy,
                TabIndex = tabIndex++,
                Content = Label(action.Label)
            };
            AutomationProperties.SetName(button, action.Label);
            AutomationProperties.SetAutomationId(button, action.Id);
            button.Margin = new Thickness(0, 0, _gap, _gap);
            button.Click += async (_, _) => await action.InvokeAsync(CancellationToken.None).ConfigureAwait(true);
            _buttons.Children.Add(button);
            _byId[action.Id] = (button, action);
        }
    }

    private TextBlock Label(string text)
    {
        TextBlock label = new() { Text = text };
        ThemeAccess.Body(label, this);
        ThemeAccess.ApplyScriptFont(label, this, text);
        return label;
    }

    /// <inheritdoc/>
    protected override AutomationPeer OnCreateAutomationPeer() => new ControlAutomationPeer(this);
}
