using Avalonia;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>Write-only secret presentation. A replacement is delegated to the host's secure workflow.
/// This control never receives, retains, copies or returns secret bytes, old or new.</summary>
public sealed class SpotSecretInput : CatalogueControl
{
    public static readonly StyledProperty<SecretFieldValue> ValueProperty = AvaloniaProperty.Register<SpotSecretInput, SecretFieldValue>(nameof(Value), new SecretFieldValue(false));
    public SecretFieldValue Value { get => GetValue(ValueProperty); set => SetValue(ValueProperty, value); }
    private readonly Avalonia.Controls.TextBox _status = Display("Secret status");
    private readonly Avalonia.Controls.Button _replace;
    public string StatusText => Value.IsSet ? "Set" : "Not set";
    public event EventHandler? ReplaceRequested;
    public SpotSecretInput() : base("Secret")
    {
        _replace = Action("Replace secret", RequestReplace);
        Body.Children.Add(_status);
        Body.Children.Add(_replace);
        UpdatePresentation();
    }
    public void RequestReplace() { if (!IsReadOnly) ReplaceRequested?.Invoke(this, EventArgs.Empty); }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == ValueProperty) UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (_replace is null) return;
        _status.Text = StatusText;
        _replace.IsVisible = !IsReadOnly;
    }
    public override string ToString() => $"Secret: {StatusText}";
}
