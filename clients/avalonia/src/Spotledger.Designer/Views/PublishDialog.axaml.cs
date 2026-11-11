using Avalonia.Controls;
using Avalonia.Interactivity;

namespace Spotledger.Client.Designer.Views;

/// <summary>The publish dialog's cancel button closes it without publishing.</summary>
public partial class PublishDialog : UserControl
{
    /// <summary>Creates the view.</summary>
    public PublishDialog() => InitializeComponent();

    private void OnCancel(object? sender, RoutedEventArgs e) =>
        (DataContext as DesignerViewModel)?.Publish.Close();
}
