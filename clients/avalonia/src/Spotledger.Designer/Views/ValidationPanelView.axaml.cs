using Avalonia.Controls;
using Avalonia.Interactivity;

namespace Spotledger.Client.Designer.Views;

/// <summary>Clicking an issue asks the designer to select the element the server named.</summary>
public partial class ValidationPanelView : UserControl
{
    /// <summary>Creates the view.</summary>
    public ValidationPanelView() => InitializeComponent();

    private void OnIssueClick(object? sender, RoutedEventArgs e)
    {
        if (DataContext is DesignerViewModel designer && (sender as Control)?.DataContext is ValidationIssue issue)
        {
            designer.Validation.SelectIssueCommand.Execute(issue);
        }
    }
}
