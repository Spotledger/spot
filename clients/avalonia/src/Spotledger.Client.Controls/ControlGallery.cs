using System.Collections.Generic;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Markup.Xaml;

namespace Spotledger.Client.Controls;

/// <summary>
/// Every design-system control in every state, in both densities and both writing
/// directions. The tests render this headlessly in Light and Dark and fail on any binding
/// error; wiring it into the Desktop app happens in a later task (the Desktop project is
/// not owned here).
/// </summary>
public sealed partial class ControlGallery : UserControl
{
    public ControlGallery()
    {
        InitializeComponent();
        foreach (var name in new[] { "ComfortableLtrSection", "CompactLtrSection", "ComfortableRtlSection", "CompactRtlSection" })
            this.FindControl<StackPanel>(name)?.Children.Add(new CatalogueGallery());
    }

    private void InitializeComponent()
    {
        AvaloniaXamlLoader.Load(this);
    }

    /// <summary>A representative option set for the selects in the gallery.</summary>
    public static IReadOnlyList<SelectOption> CustomerOptions { get; } = new List<SelectOption>
    {
        new("cust-1", "Acme Textiles / اکمی ٹیکسٹائلز"),
        new("cust-2", "Karim Traders / کریم ٹریڈرز"),
        new("cust-3", "Lahore Mills / لاہور ملز")
    }.AsReadOnly();
}
