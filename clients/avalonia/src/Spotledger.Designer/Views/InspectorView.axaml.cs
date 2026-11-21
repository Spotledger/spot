using Avalonia.Controls;
using Avalonia.Interactivity;

namespace Spotledger.Client.Designer.Views;

/// <summary>
/// The inspector's event wiring: the rows that need their own item (a choice to move, a condition
/// row to remove) and the checkbox whose click sends the required flag to the server.
/// </summary>
public partial class InspectorView : UserControl
{
    /// <summary>Creates the view.</summary>
    public InspectorView() => InitializeComponent();

    private InspectorViewModel? Inspector => (DataContext as DesignerViewModel)?.Inspector;

    private void OnDismissTypeChange(object? sender, RoutedEventArgs e) => Inspector?.DismissTypeChange();

    private async void OnRequiredClick(object? sender, RoutedEventArgs e)
    {
        if (Inspector is { } inspector)
        {
            inspector.ToggleRequiredCommand.Execute(null);
            await inspector.ToggleRequiredCommand.ExecutionTask;
        }
    }

    private void OnOptionUp(object? sender, RoutedEventArgs e) => MoveChoice(sender, up: true);

    private void OnOptionDown(object? sender, RoutedEventArgs e) => MoveChoice(sender, up: false);

    private void OnOptionRemove(object? sender, RoutedEventArgs e)
    {
        if (Inspector is { } inspector && (sender as Control)?.Tag is OptionRow row)
        {
            inspector.RemoveOptionCommand.Execute(row);
        }
    }

    private void OnConditionRowRemove(object? sender, RoutedEventArgs e)
    {
        if (Inspector is { } inspector && (sender as Control)?.Tag is ConditionRowDraft row)
        {
            inspector.RemoveConditionRowCommand.Execute(row);
        }
    }

    private void MoveChoice(object? sender, bool up)
    {
        if (Inspector is not { } inspector || (sender as Control)?.Tag is not OptionRow row)
        {
            return;
        }

        var command = up ? inspector.MoveOptionUpCommand : inspector.MoveOptionDownCommand;
        command.Execute(row);
    }
}
