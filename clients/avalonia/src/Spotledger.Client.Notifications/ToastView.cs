using Avalonia;
using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Avalonia.Threading;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Notifications;

/// <summary>
/// Shows <see cref="ToastTray.Current"/> as one card. It never takes keyboard focus; hover and focus
/// within it hold the countdown. The card is a polite live region so a screen reader announces it.
/// </summary>
public sealed class ToastView : UserControl, IDisposable
{
    private readonly ToastTray _queue;
    private readonly List<IDisposable> _subscriptions = [];

    /// <summary>Creates the view over a queue.</summary>
    public ToastView(ToastTray queue)
    {
        _queue = queue ?? throw new ArgumentNullException(nameof(queue));
        Focusable = false;
        IsVisible = false;
        MaxWidth = 380;
        HorizontalAlignment = HorizontalAlignment.Right;
        VerticalAlignment = VerticalAlignment.Bottom;
        Margin = new Thickness(16);
        AutomationProperties.SetLiveSetting(this, AutomationLiveSetting.Polite);
        PointerEntered += (_, _) => _queue.Pause();
        PointerExited += (_, _) => _queue.Resume();
        GotFocus += (_, _) => _queue.Pause();
        LostFocus += (_, _) => _queue.Resume();
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorTextPrimary, brush => Foreground = brush));
        _subscriptions.Add(NoticeLook.BindBrush(this, Tokens.TokenColorSurfaceRaised, brush => Background = brush));
        _queue.CurrentChanged += OnChanged;
        Render();
    }

    /// <inheritdoc/>
    public void Dispose()
    {
        _queue.CurrentChanged -= OnChanged;
        foreach (var subscription in _subscriptions)
        {
            subscription.Dispose();
        }
    }

    private void OnChanged(object? sender, EventArgs e) => Dispatcher.UIThread.Post(Render);

    private void Render()
    {
        var notice = _queue.Current;
        IsVisible = notice is not null;
        if (notice is null)
        {
            Content = null;
            AutomationProperties.SetName(this, string.Empty);
            return;
        }

        var marker = new Avalonia.Controls.Shapes.Path
        {
            Data = Geometry.Parse(NoticeLook.Shape(notice.Severity)),
            StrokeThickness = 2,
            Width = 20,
            Height = 20,
            Stretch = Stretch.Uniform,
            VerticalAlignment = VerticalAlignment.Top,
            Margin = new Thickness(0, 2, 8, 0),
        };
        _subscriptions.Add(NoticeLook.BindBrush(marker, NoticeLook.ColourToken(notice.Severity), brush => marker.Stroke = brush));
        var title = NoticeLook.Text(notice.Title, 2);
        title.FontWeight = FontWeight.SemiBold;
        var text = new StackPanel { Spacing = 2, Children = { title } };
        if (notice.Body.Length > 0)
        {
            text.Children.Add(NoticeLook.Text(notice.Body, 3, 13));
        }

        var close = new Button { Content = "Dismiss", Focusable = false };
        close.Click += (_, _) => _queue.Dismiss();
        DockPanel.SetDock(marker, Dock.Left);
        DockPanel.SetDock(close, Dock.Right);
        Content = new Border
        {
            Padding = new Thickness(12),
            CornerRadius = new CornerRadius(6),
            BorderThickness = new Thickness(1),
            Child = new DockPanel { Children = { marker, close, text } },
        };
        AutomationProperties.SetName(this, $"{NoticeLook.Name(notice.Severity)}: {notice.Title}");
    }
}
