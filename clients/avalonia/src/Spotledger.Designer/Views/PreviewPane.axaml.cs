using System.Globalization;
using Avalonia.Controls;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Designer.Views;

/// <summary>
/// Builds the preview's visual tree from the server's preview metadata: one block per section, one
/// row per field, and for each field the real control from <see cref="FieldControlFactory"/> bound
/// to the board's sample value. The preview is the form, not a picture of one.
/// </summary>
public partial class PreviewPane : UserControl
{
    private PreviewViewModel? _preview;

    /// <summary>Creates the view.</summary>
    public PreviewPane()
    {
        InitializeComponent();
        DataContextChanged += (_, _) => Attach();
        AttachedToVisualTree += (_, _) => Attach();
    }

    private void Attach()
    {
        var next = (DataContext as DesignerViewModel)?.Preview;
        if (ReferenceEquals(next, _preview))
        {
            return;
        }

        if (_preview is not null)
        {
            _preview.PreviewChanged -= OnPreviewChanged;
        }

        _preview = next;
        if (_preview is not null)
        {
            _preview.PreviewChanged += OnPreviewChanged;
        }

        Render();
    }

    private void OnPreviewChanged(object? sender, EventArgs e) => Render();

    private async void OnDesktop(object? sender, RoutedEventArgs e) => await ShowAsync(PreviewLayout.Desktop);

    private async void OnNarrow(object? sender, RoutedEventArgs e) => await ShowAsync(PreviewLayout.Narrow);

    private async void OnList(object? sender, RoutedEventArgs e) => await ShowAsync(PreviewLayout.List);

    private async void OnRefresh(object? sender, RoutedEventArgs e)
    {
        if (_preview is { } preview)
        {
            await preview.ShowAsync(preview.CurrentLayout);
        }
    }

    private async Task ShowAsync(PreviewLayout layout)
    {
        if (_preview is { } preview)
        {
            await preview.ShowAsync(layout);
        }
    }

    /// <summary>Rebuilds the pane from the last preview the server built.</summary>
    public void Render()
    {
        Host.Children.Clear();
        if (_preview?.Board is not { } board || _preview.Preview is not { } model)
        {
            Host.Children.Add(Hint("Press Refresh to preview the form with sample values."));
            return;
        }

        if (_preview.IsList)
        {
            Host.Children.Add(ListView(board, model));
            return;
        }

        foreach (var section in model.FormLayout.SectionList)
        {
            Host.Children.Add(Section(board, section));
        }

        foreach (var field in model.FieldList.Where(f => !model.FormLayout.PlacedFields().Contains(f.Name, StringComparer.Ordinal)))
        {
            if (board.Model.Field(field.Name) is { } modelField)
            {
                Host.Children.Add(FieldRow(board, modelField));
            }
        }
    }

    private static Border Section(PreviewBoard board, SectionSnapshot section)
    {
        var fields = new StackPanel { Spacing = 4 };
        var columns = section.Columns ?? 1;
        var grid = new Grid
        {
            ColumnDefinitions = new ColumnDefinitions(string.Join(',', Enumerable.Repeat("*", Math.Clamp(columns, 1, 4)))),
        };
        var index = 0;
        foreach (var name in section.FieldItems)
        {
            if (board.Model.Field(name) is not { } field)
            {
                continue;
            }

            var row = FieldRow(board, field);
            Grid.SetColumn(row, index % Math.Clamp(columns, 1, 4));
            grid.Children.Add(row);
            index++;
        }

        fields.Children.Add(grid);
        return new Border
        {
            Padding = new Avalonia.Thickness(8),
            CornerRadius = new Avalonia.CornerRadius(4),
            Child = new StackPanel
            {
                Spacing = 6,
                Children =
                {
                    new TextBlock { Text = section.Title, FontWeight = FontWeight.SemiBold },
                    fields,
                },
            },
        };
    }

    private static StackPanel FieldRow(PreviewBoard board, FieldModel field)
    {
        var state = board.State(field.Name);
        Control input = state is null
            ? new TextBox { IsReadOnly = true, Text = "(no sample)" }
            : FieldControlFactory.Create(field, state);
        input.HorizontalAlignment = HorizontalAlignment.Stretch;

        var label = new TextBlock
        {
            Text = field.Required ? $"{field.Label} *" : field.Label,
            FontSize = 11,
            Opacity = 0.75,
        };
        return new StackPanel { Spacing = 2, Margin = new Avalonia.Thickness(0, 2), Children = { label, input } };
    }

    private static StackPanel ListView(PreviewBoard board, PreviewModel model)
    {
        var rows = new StackPanel { Spacing = 2 };
        var header = new Grid { ColumnDefinitions = new ColumnDefinitions("200,*") };
        header.Children.Add(new TextBlock { Text = "Column", FontWeight = FontWeight.SemiBold });
        var sample = new TextBlock { Text = "Sample value", FontWeight = FontWeight.SemiBold };
        Grid.SetColumn(sample, 1);
        header.Children.Add(sample);
        rows.Children.Add(header);

        foreach (var field in model.FieldList)
        {
            var row = new Grid { ColumnDefinitions = new ColumnDefinitions("200,*") };
            row.Children.Add(new TextBlock { Text = field.Label });
            var value = board.State(field.Name)?.Value;
            var text = new TextBlock
            {
                Text = SampleText(value),
                Opacity = 0.85,
            };
            Grid.SetColumn(text, 1);
            row.Children.Add(text);
            rows.Children.Add(row);
        }

        return rows;
    }

    /// <summary>A field value as the list view shows it (never a secret's contents).</summary>
    private static string SampleText(FieldValue? value) => value switch
    {
        null => string.Empty,
        TextFieldValue text => text.Value,
        IntFieldValue number => number.Value.ToString(CultureInfo.InvariantCulture),
        DecimalFieldValue number => number.CanonicalText,
        BoolFieldValue flag => flag.Value ? "true" : "false",
        DateFieldValue date => date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
        SelectFieldValue option => option.Value,
        LinkFieldValue link => link.DisplayText ?? link.Id ?? string.Empty,
        AttachFieldValue file => file.FileId ?? string.Empty,
        SecretFieldValue secret => secret.IsSet ? "Set" : "Not set",
        _ => value.GetType().Name,
    };

    private static TextBlock Hint(string text) => new TextBlock
    {
        Text = text,
        TextWrapping = TextWrapping.Wrap,
        Opacity = 0.7,
    };
}
