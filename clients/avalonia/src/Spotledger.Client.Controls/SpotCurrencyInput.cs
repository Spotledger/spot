using Avalonia.Controls;
using Avalonia.Media;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>An exact-text decimal editor: even 38-digit amounts stay in the Metadata parser.
/// CurrencyInfo is server presentation, never resolved here or inserted into edit text.</summary>
public sealed class SpotCurrencyInput : CatalogueControl
{
    public SpotTextInput Editor { get; } = new();
    private readonly TextBlock _symbol = new();
    private CurrencyInfo? _currency;
    public CurrencyInfo? CurrencyInfo
    {
        get => _currency;
        set { _currency = value; UpdatePresentation(); }
    }
    public string? Text { get => Editor.Text; set => Editor.Text = value; }
    public string? ErrorText { get => Editor.ErrorText; set => Editor.ErrorText = value; }
    public SpotCurrencyInput() : base("Currency")
    {
        var row = new DockPanel();
        DockPanel.SetDock(_symbol, Dock.Left);
        row.Children.Add(_symbol);
        row.Children.Add(Editor);
        Body.Children.Add(row);
        Editor.TemplateApplied += (_, _) =>
        {
            if (Editor.TextBoxControl is { } text) text.TextAlignment = TextAlignment.Right;
        };
        UpdatePresentation();
    }
    protected override void UpdatePresentation()
    {
        if (Editor is null) return;
        Editor.IsReadOnly = IsReadOnly;
        _symbol.Text = _currency?.Symbol;
        _symbol.IsVisible = _currency is not null;
    }
}
