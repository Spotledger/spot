using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Interactivity;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class TableControlTests
{
    [AvaloniaFact]
    public void GenericRowsCommitEditsAndRemoveExactlyOneUnsavedRow()
    {
        var model = new DocTypeModel("row", "Row", 1, false, [new("note", "Note", FieldKind.Text)]);
        var input = new SpotTableControl { Value = new TableFieldValue([
            TableRowValue.New(new Dictionary<string, FieldValue> { ["note"] = new TextFieldValue("first") }),
            TableRowValue.New(new Dictionary<string, FieldValue> { ["note"] = new TextFieldValue("second") })], model) };
        var window = TestHost.Show(input);
        var editors = input.GetVisualDescendants().OfType<SpotTextInput>().ToArray();
        Assert.Equal(2, editors.Length);
        editors[1].Text = "edited";
        var remove = input.GetVisualDescendants().OfType<Button>().First(x => Equals(x.Content, "Remove row"));
        remove.RaiseEvent(new RoutedEventArgs(Button.ClickEvent));
        var remaining = Assert.Single(input.Value.Rows);
        Assert.Equal("edited", Assert.IsType<TextFieldValue>(remaining.Values["note"]).Value);
        window.Close();
    }
}
