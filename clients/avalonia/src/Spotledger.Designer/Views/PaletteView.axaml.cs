using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;

namespace Spotledger.Client.Designer.Views;

/// <summary>
/// The palette's pointer behaviour: a press picks the type and starts a drag that carries the
/// server's type name to the canvas; a click on its own only picks the type, so the keyboard's
/// insert gesture and the Insert button know what to add.
/// </summary>
public partial class PaletteView : UserControl
{
    /// <summary>The drag payload prefix for a field type (the rest is the server's type name).</summary>
    public const string TypePayloadPrefix = "spotledger.field-type:";

    /// <summary>Creates the view.</summary>
    public PaletteView() => InitializeComponent();

    private async void OnEntryPressed(object? sender, PointerPressedEventArgs e)
    {
        if (sender is not Control { DataContext: FieldTypeInfo info } || DataContext is not DesignerViewModel designer)
        {
            return;
        }

        designer.SelectedPaletteType = info.Type;
        if (!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)
        {
            return;
        }

        var transfer = new DataTransfer();
        transfer.Add(DataTransferItem.CreateText(TypePayloadPrefix + info.Type));
        await DragDrop.DoDragDropAsync(e, transfer, DragDropEffects.Copy);
    }
}
