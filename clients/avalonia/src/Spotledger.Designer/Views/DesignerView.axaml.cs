using Avalonia.Controls;
using Avalonia.Interactivity;

namespace Spotledger.Client.Designer.Views;

/// <summary>
/// The designer's own view: the toolbar's draft-lifecycle commands, and the one place a host binds
/// a draft to. Everything else is a panel that reads the same view-model.
/// </summary>
public partial class DesignerView : UserControl
{
    /// <summary>Creates the view.</summary>
    public DesignerView() => InitializeComponent();

    private DesignerViewModel? Designer => DataContext as DesignerViewModel;

    private async void OnUndo(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.UndoCommand);

    private async void OnRedo(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.RedoCommand);

    private async void OnSave(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.SaveCommand);

    private async void OnDiscard(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.DiscardCommand);

    private async void OnInsert(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.InsertCommand);

    private async void OnDelete(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.DeleteCommand);

    private async void OnPublish(object? sender, RoutedEventArgs e) => await RunAsync(Designer?.PublishDialogCommand);

    private static async Task RunAsync(AsyncRelayCommand? command)
    {
        if (command is null || !command.CanExecute(null))
        {
            return;
        }

        command.Execute(null);
        await command.ExecutionTask;
    }
}
