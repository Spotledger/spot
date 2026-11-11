using System.Windows.Input;
using Avalonia;
using Avalonia.Controls;
using Avalonia.VisualTree;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>Server text only. The app owns reveal transport and the separate draft result.
/// No editor, value, state, or draft reference is retained here.</summary>
public sealed class SpotMaskedField : CatalogueControl
{
    private readonly TextBox _text = Display("Masked value");
    private readonly Button _reveal;
    private string _maskedText = string.Empty;
    private bool _canReveal;
    private readonly List<Visual> _ancestors = [];
    public ICommand? RevealCommand { get; set; }
    public object? CommandParameter { get; set; }
    public bool IsRevealed { get; private set; }
    public string DisplayText => _text.Text ?? string.Empty;
    public bool CanReveal => _canReveal;

    public SpotMaskedField() : base("Masked field")
    {
        _text.IsUndoEnabled = false;
        Body.Children.Add(new TextBlock { Text = "🔒", Focusable = false });
        Body.Children.Add(_text);
        _reveal = Action("Reveal", RequestReveal);
        Body.Children.Add(_reveal);
        UpdatePresentation();
    }

    /// <summary>Explicit refresh after app-layer reads/reveals. Only formatted text is copied.</summary>
    public void Load(FieldState state)
    {
        ArgumentNullException.ThrowIfNull(state);
        _maskedText = state.MaskedText ?? string.Empty;
        _canReveal = state.IsMasked && state.CanReveal;
        HideReveal();
        if (_canReveal && state.RevealedValue is { } revealed && IsEffectivelyVisible && TopLevel.GetTopLevel(this) is not null)
        {
            _text.Text = FieldControlFactory.FormatValue(revealed);
            IsRevealed = true;
        }
        UpdatePresentation();
    }

    public void RequestReveal()
    {
        if (_canReveal && IsEffectivelyVisible && !IsRevealed && RevealCommand?.CanExecute(CommandParameter) == true)
            RevealCommand.Execute(CommandParameter);
    }

    /// <summary>Scrubs the only displayed copy; showing again never restores the reveal.</summary>
    public void HideReveal()
    {
        IsRevealed = false;
        _text.Text = _maskedText;
        _text.SelectionStart = _text.SelectionEnd = 0;
        UpdatePresentation();
    }

    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == IsVisibleProperty && !IsVisible) HideReveal();
    }
    protected override void OnAttachedToVisualTree(VisualTreeAttachmentEventArgs e)
    {
        base.OnAttachedToVisualTree(e);
        foreach (var ancestor in this.GetVisualAncestors())
        {
            _ancestors.Add(ancestor);
            ancestor.PropertyChanged += AncestorChanged;
        }
    }
    private void AncestorChanged(object? sender, AvaloniaPropertyChangedEventArgs change)
    {
        if (change.Property == IsVisibleProperty && sender is Visual { IsVisible: false }) HideReveal();
    }
    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e)
    {
        HideReveal();
        foreach (var ancestor in _ancestors) ancestor.PropertyChanged -= AncestorChanged;
        _ancestors.Clear();
        base.OnDetachedFromVisualTree(e);
    }
    protected override void UpdatePresentation()
    {
        if (_reveal is not null) _reveal.IsVisible = _canReveal && !IsRevealed;
    }
}
