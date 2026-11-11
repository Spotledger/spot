using System.Collections.Specialized;
using System.ComponentModel;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Styling;
using Avalonia.VisualTree;

namespace Spotledger.Client.Workspace;

internal static class TokenBinding
{
    /// <summary>Binds a brush property to a theme colour token, following the theme variant.</summary>
    public static void Brush(StyledElement element, AvaloniaProperty property, string key)
        => element.Bind(property, element.GetResourceObservable(key, value => value is Color color ? new SolidColorBrush(color) : value));

    public static TextBlock Text(string? text = null, string colour = "Color.Text.Primary", double size = 0, FontWeight weight = FontWeight.Normal)
    {
        var block = new TextBlock { Text = text, FontWeight = weight, TextWrapping = TextWrapping.Wrap, VerticalAlignment = VerticalAlignment.Center };
        if (size > 0)
        {
            block.FontSize = size;
        }

        Brush(block, TextBlock.ForegroundProperty, colour);
        return block;
    }
}

/// <summary>
/// The non-modal bar above a form that offers "Restore unsaved changes from ..." and "Discard", and,
/// when the server's version moved on, lists what changed on the server first. Built in code from the
/// theme's tokens; every colour is a resource, so light, dark and right-to-left follow the host.
/// </summary>
public sealed class RecoveryBarView : Border
{
    private readonly TextBlock _message = TokenBinding.Text();
    private readonly Button _restore = new();
    private readonly Button _discard = new();
    private readonly StackPanel _changes = new() { Spacing = 4 };
    private readonly Button _applyAll = new() { Content = "Apply all my changes" };
    private readonly Button _applyUntouched = new() { Content = "Keep the server's changes" };
    private readonly StackPanel _review;
    private RecoveryBarViewModel? _model;

    /// <summary>Creates the bar; set <c>DataContext</c> to a <see cref="RecoveryBarViewModel"/>.</summary>
    public RecoveryBarView()
    {
        TokenBinding.Brush(this, BorderBrushProperty, "Color.Border.Default");
        TokenBinding.Brush(this, BackgroundProperty, "Color.Surface.Raised");
        BorderThickness = new Thickness(0, 0, 0, 1);
        Padding = new Thickness(12, 8);
        AutomationProperties.SetName(this, "Unsaved changes found");
        AutomationProperties.SetLiveSetting(_message, AutomationLiveSetting.Polite);

        var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, Children = { _message, _restore, _discard } };
        _review = new StackPanel
        {
            Spacing = 8,
            Children =
            {
                _changes,
                new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12, Children = { _applyAll, _applyUntouched } },
            },
        };
        Child = new StackPanel { Spacing = 8, Children = { row, _review } };
        DataContextChanged += (_, _) => Attach(DataContext as RecoveryBarViewModel);
    }

    private void Attach(RecoveryBarViewModel? model)
    {
        if (_model is not null)
        {
            _model.PropertyChanged -= OnChanged;
        }

        _model = model;
        if (model is null)
        {
            return;
        }

        model.PropertyChanged += OnChanged;
        _message.Text = model.Message;
        _restore.Content = model.RestoreLabel;
        _restore.Command = model.RestoreCommand;
        _discard.Content = model.DiscardLabel;
        _discard.Command = model.DiscardCommand;
        _applyAll.Command = model.ApplyAllCommand;
        _applyUntouched.Command = model.ApplyUntouchedCommand;
        _changes.Children.Clear();
        foreach (var item in model.ServerChanges)
        {
            _changes.Children.Add(TokenBinding.Text($"{item.Label}: yours \"{item.YourValue}\", on the server now \"{item.ServerValue}\"", "Color.Text.Secondary"));
        }

        Sync();
    }

    private void OnChanged(object? sender, PropertyChangedEventArgs e) => Sync();

    private void Sync()
    {
        IsVisible = _model?.IsVisible == true;
        _review.IsVisible = _model?.IsReviewing == true;
    }
}

/// <summary>The "Recover drafts" list: one row per draft with Open and Discard, and Discard all.</summary>
public sealed class RecoverDraftsView : Border
{
    private readonly StackPanel _rows = new() { Spacing = 4 };
    private readonly TextBlock _title = TokenBinding.Text(null, "Color.Text.Primary", 18, FontWeight.SemiBold);
    private readonly TextBlock _empty = TokenBinding.Text(null, "Color.Text.Secondary");
    private readonly Button _discardAll = new() { Content = "Discard all" };
    private RecoverDraftsViewModel? _model;

    /// <summary>Creates the list; set <c>DataContext</c> to a <see cref="RecoverDraftsViewModel"/>.</summary>
    public RecoverDraftsView()
    {
        Padding = new Thickness(16);
        AutomationProperties.SetName(this, "Recover drafts");
        TokenBinding.Brush(this, BackgroundProperty, "Color.Surface.Canvas");
        Child = new StackPanel { Spacing = 8, Children = { _title, _empty, _rows, _discardAll } };
        DataContextChanged += (_, _) => Attach(DataContext as RecoverDraftsViewModel);
    }

    private void Attach(RecoverDraftsViewModel? model)
    {
        if (_model is not null)
        {
            _model.Items.CollectionChanged -= OnItemsChanged;
        }

        _model = model;
        if (model is null)
        {
            return;
        }

        model.Items.CollectionChanged += OnItemsChanged;
        _title.Text = model.Title;
        _empty.Text = model.EmptyText;
        _discardAll.Command = model.DiscardAllCommand;
        Rebuild();
    }

    private void OnItemsChanged(object? sender, NotifyCollectionChangedEventArgs e) => Rebuild();

    private void Rebuild()
    {
        _rows.Children.Clear();
        if (_model is null)
        {
            return;
        }

        foreach (var item in _model.Items)
        {
            _rows.Children.Add(Row(item));
        }

        _empty.IsVisible = _model.Items.Count == 0;
        _discardAll.IsVisible = _model.Items.Count > 0;
    }

    private DockPanel Row(RecoverDraftItem item)
    {
        var text = new StackPanel
        {
            Children =
            {
                TokenBinding.Text(item.Document),
                TokenBinding.Text(item.TimeText, "Color.Text.Secondary", 12),
            },
        };
        var open = new Button { Content = "Open" };
        open.Click += (_, _) => _model?.OpenCommand.Execute(item);
        var discard = new Button { Content = "Discard" };
        discard.Click += (_, _) => _model?.DiscardCommand.Execute(item);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, Children = { open, discard } };
        var panel = new DockPanel { Margin = new Thickness(0, 4) };
        DockPanel.SetDock(buttons, Dock.Right);
        panel.Children.Add(buttons);
        panel.Children.Add(text);
        return panel;
    }
}
