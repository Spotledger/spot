using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;

namespace Spotledger.Client.Controls;

/// <summary>Precompiled composition shared by catalogue controls; no runtime XAML or metadata rules.</summary>
public abstract class CatalogueControl : UserControl
{
    public static readonly StyledProperty<bool> IsReadOnlyProperty = AvaloniaProperty.Register<CatalogueControl, bool>(nameof(IsReadOnly));
    public bool IsReadOnly { get => GetValue(IsReadOnlyProperty); set => SetValue(IsReadOnlyProperty, value); }
    protected StackPanel Body { get; } = new();
    protected CatalogueControl(string name)
    {
        Content = Body;
        AutomationProperties.SetName(this, name);
    }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == IsReadOnlyProperty) UpdatePresentation();
    }
    protected abstract void UpdatePresentation();
    protected override AutomationPeer OnCreateAutomationPeer() => new CataloguePeer(this);
    private sealed class CataloguePeer(CatalogueControl owner) : ControlAutomationPeer(owner)
    {
        protected override AutomationControlType GetAutomationControlTypeCore() => AutomationControlType.Group;
        protected override string GetClassNameCore() => Owner.GetType().Name;
    }
    protected static TextBox Display(string name)
    {
        var text = new TextBox { IsReadOnly = true };
        AutomationProperties.SetName(text, name);
        return text;
    }
    protected static Button Action(string name, Action action)
    {
        var button = new Button { Content = name };
        AutomationProperties.SetName(button, name);
        button.Click += (_, _) => action();
        return button;
    }
}
