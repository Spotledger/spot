using Avalonia;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.VisualTree;

namespace Spotledger.Client.Designer.Views;

/// <summary>
/// The canvas's pointer and keyboard behaviour. A drag carries either a palette type or a field;
/// the element under the pointer decides the op: a type onto a section adds a field there, onto a
/// field inserts it after that field, a field onto a field reorders, and a drop on the tray takes
/// the field off the form. Every one of those actions also has a key gesture.
/// </summary>
public partial class CanvasView : UserControl
{
    /// <summary>The drag payload prefix for a field already on the form.</summary>
    public const string FieldPayloadPrefix = "spotledger.field:";

    /// <summary>Creates the view and wires the drop handling once for the whole canvas.</summary>
    public CanvasView()
    {
        InitializeComponent();
        AddHandler(DragDrop.DragOverEvent, OnDragOver, RoutingStrategies.Bubble, handledEventsToo: true);
        AddHandler(DragDrop.DropEvent, OnDrop, RoutingStrategies.Bubble, handledEventsToo: true);
    }

    private DesignerViewModel? Designer => DataContext as DesignerViewModel;

    // ---------------------------------------------------------------- pointer

    private async void OnFieldPressed(object? sender, PointerPressedEventArgs e)
    {
        if (Designer is not { } designer || sender is not Control { DataContext: CanvasField field })
        {
            return;
        }

        designer.Canvas.SelectedField = field;
        if (!e.GetCurrentPoint(this).Properties.IsLeftButtonPressed)
        {
            return;
        }

        var transfer = new DataTransfer();
        transfer.Add(DataTransferItem.CreateText(Field(field)));
        await DragDrop.DoDragDropAsync(e, transfer, DragDropEffects.Move);
    }

    private void OnDragOver(object? sender, DragEventArgs e) =>
        e.DragEffects = TryRead(e, out _, out _) ? DragDropEffects.Move : DragDropEffects.None;

    private async void OnDrop(object? sender, DragEventArgs e)
    {
        if (Designer is not { } designer || !TryRead(e, out var kind, out var value))
        {
            return;
        }

        e.Handled = true;
        var target = TargetAt(e.Source as Visual);
        if (target.Field is { } field)
        {
            if (designer.Canvas.CurrentUnplaced.Contains(field))
            {
                await DropOnTrayAsync(designer, kind, value);
                return;
            }

            var section = designer.Canvas.CurrentSections.FirstOrDefault(s => s.Fields.Contains(field));
            if (section is not null)
            {
                await DeliverAsync(designer, kind, value, section.Id, field.Name);
            }

            return;
        }

        if (target.Section is { } sectionRow)
        {
            await DeliverAsync(designer, kind, value, sectionRow.Id, null);
            return;
        }

        if (target.IsTray)
        {
            await DropOnTrayAsync(designer, kind, value);
        }
    }

    private static async Task DropOnTrayAsync(DesignerViewModel designer, PayloadKind kind, string value)
    {
        if (kind == PayloadKind.Type)
        {
            await designer.DropAsync(value, string.Empty);
            return;
        }

        if (designer.Canvas.CurrentUnplaced.FirstOrDefault(f => Matches(f, value)) is { } field)
        {
            await designer.Canvas.UnplaceAsync(field);
        }
    }

    /// <summary>Routes a drop to the command for its payload and target.</summary>
    private static async Task DeliverAsync(DesignerViewModel designer, PayloadKind kind, string value, string sectionId, string? afterField)
    {
        if (kind == PayloadKind.Type)
        {
            await designer.DropAsync(value, sectionId, afterField);
            return;
        }

        if (designer.Canvas.CurrentSections.SelectMany(s => s.Fields).FirstOrDefault(f => Matches(f, value)) is { } field)
        {
            await designer.Canvas.PlaceAsync(field, sectionId, afterField);
        }
    }

    // ---------------------------------------------------------------- keyboard

    private async void OnKeyDown(object? sender, KeyEventArgs e)
    {
        if (Designer is not { } designer)
        {
            return;
        }

        if (e.Key == Key.Delete && designer.Canvas.SelectedField is not null)
        {
            await designer.Canvas.RemoveSelectedAsync();
            e.Handled = true;
            return;
        }

        if (GestureOf(e) is not { } gesture)
        {
            return;
        }

        var (handled, _) = await designer.HandleGestureAsync(gesture);
        e.Handled = handled;
    }

    /// <summary>The gesture name for a key event, in the canvas map's vocabulary.</summary>
    public static string? GestureOf(KeyEventArgs e)
    {
        var key = e.Key switch
        {
            Key.Up => "Up",
            Key.Down => "Down",
            Key.Left => "Left",
            Key.Right => "Right",
            Key.I => "I",
            Key.Insert => "Insert",
            _ => null,
        };
        if (key is null)
        {
            return null;
        }

        var parts = new List<string>(4);
        if (e.KeyModifiers.HasFlag(KeyModifiers.Control))
        {
            parts.Add("Ctrl");
        }

        if (e.KeyModifiers.HasFlag(KeyModifiers.Alt))
        {
            parts.Add("Alt");
        }

        if (e.KeyModifiers.HasFlag(KeyModifiers.Shift))
        {
            parts.Add("Shift");
        }

        parts.Add(key);
        return string.Join('+', parts);
    }

    // ---------------------------------------------------------------- sections

    private async void OnAddSection(object? sender, RoutedEventArgs e)
    {
        if (Designer is { } designer)
        {
            await designer.Canvas.AddSectionAsync(designer.NewSectionTitle);
        }
    }

    private async void OnAddTab(object? sender, RoutedEventArgs e)
    {
        if (Designer is { } designer)
        {
            await designer.Canvas.AddTabAsync(designer.NewSectionTitle);
        }
    }

    private void OnOpenChildGrid(object? sender, RoutedEventArgs e)
    {
        if (Designer is { } designer && sender is Control { Tag: string doctype })
        {
            designer.Canvas.OpenChildGrid(doctype);
        }
    }

    private void OnCloseChildGrid(object? sender, RoutedEventArgs e) => Designer?.Canvas.CloseChildGrid();

    private async void OnNarrower(object? sender, RoutedEventArgs e) => await ResizeAsync(sender, -1);

    private async void OnWider(object? sender, RoutedEventArgs e) => await ResizeAsync(sender, 1);

    private async Task ResizeAsync(object? sender, int delta)
    {
        if (Designer is not { } designer)
        {
            return;
        }

        var section = (sender as Control)?.Tag as CanvasSectionViewModel;
        if (section is null)
        {
            return;
        }

        await designer.Canvas.SetColumnsAsync(section, section.Columns + delta);
    }

    // ---------------------------------------------------------------- targets and payloads

    /// <summary>What the pointer is over when a drop lands.</summary>
    private readonly record struct DropTarget(CanvasField? Field, CanvasSectionViewModel? Section, bool IsTray);

    /// <summary>
    /// The drop target under the pointer: the closest field, else the closest section, else the
    /// tray. The visual tree is walked because the drop event bubbles from the element the pointer
    /// is over, and that element carries the item as its data context.
    /// </summary>
    private DropTarget TargetAt(Visual? source)
    {
        for (var visual = source; visual is not null; visual = visual.GetVisualParent())
        {
            if (visual is not Control control)
            {
                continue;
            }

            if (ReferenceEquals(control, Tray))
            {
                return new DropTarget(null, null, true);
            }

            if (control.DataContext is CanvasField field)
            {
                return new DropTarget(field, null, false);
            }

            if (control.DataContext is CanvasSectionViewModel section)
            {
                return new DropTarget(null, section, false);
            }
        }

        return new DropTarget(null, null, false);
    }

    private enum PayloadKind
    {
        Type,
        Field,
    }

    private static string Field(CanvasField field) =>
        FieldPayloadPrefix + field.Name + "|" + (field.ChildDoctype ?? string.Empty);

    private static bool Matches(CanvasField field, string payload) =>
        string.Equals(payload, field.Name + "|" + (field.ChildDoctype ?? string.Empty), StringComparison.Ordinal);

    private static bool TryRead(DragEventArgs e, out PayloadKind kind, out string value)
    {
        kind = PayloadKind.Type;
        value = string.Empty;
        if (e.DataTransfer.TryGetText() is not { Length: > 0 } text)
        {
            return false;
        }

        if (text.StartsWith(PaletteView.TypePayloadPrefix, StringComparison.Ordinal))
        {
            kind = PayloadKind.Type;
            value = text[PaletteView.TypePayloadPrefix.Length..];
            return value.Length > 0;
        }

        if (text.StartsWith(FieldPayloadPrefix, StringComparison.Ordinal))
        {
            kind = PayloadKind.Field;
            value = text[FieldPayloadPrefix.Length..];
            return value.Length > 0;
        }

        return false;
    }
}
