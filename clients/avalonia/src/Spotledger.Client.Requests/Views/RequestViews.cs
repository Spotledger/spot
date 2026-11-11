using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;
using Spotledger.Client.Forms;

namespace Spotledger.Client.Requests;

/// <summary>The create button of a list or the home page: "New", or "Start X" for a process-only
/// type. The shortcut is the same for both.</summary>
public static class CreateButton
{
    /// <summary>Builds the button for <paramref name="action"/>; <paramref name="invoke"/> runs when it is pressed or the shortcut is used.</summary>
    public static Button Create(CreateAction action, Action invoke)
    {
        ArgumentNullException.ThrowIfNull(action);
        ArgumentNullException.ThrowIfNull(invoke);
        var button = new Button { Content = action.Label, HotKey = KeyGesture.Parse(action.Gesture) };
        AutomationProperties.SetName(button, action.Label);
        button.Click += (_, _) => invoke();
        return button;
    }
}

/// <summary>My requests: the three views as buttons, and the rows with their badges.</summary>
public sealed class MyRequestsView : UserControl
{
    private readonly MyRequestsViewModel _model;
    private readonly Func<DateTimeOffset> _now;
    private readonly StackPanel _filters = new() { Orientation = Orientation.Horizontal, Spacing = 8 };
    private readonly StackPanel _rows = new() { Spacing = 4 };
    private readonly TextBlock _empty = new() { Opacity = 0.7 };
    private readonly TextBlock _error = new() { IsVisible = false };

    /// <summary>Creates the view. <paramref name="now"/> is the host's clock, read only when the person acts.</summary>
    public MyRequestsView(MyRequestsViewModel model, Func<DateTimeOffset> now)
    {
        _model = model ?? throw new ArgumentNullException(nameof(model));
        _now = now ?? throw new ArgumentNullException(nameof(now));
        FlowDirection = model.Text.FlowDirection;
        var title = new TextBlock { Text = model.Title, FontSize = 20, FontWeight = FontWeight.SemiBold };
        AutomationProperties.SetName(this, model.Title);
        foreach (var filter in Enum.GetValues<RequestFilter>())
        {
            var button = new Button { Content = model.FilterLabel(filter) };
            button.Click += async (_, _) => await model.SelectFilterAsync(filter, _now()).ConfigureAwait(true);
            _filters.Children.Add(button);
        }

        _empty.Text = model.EmptyText;
        Content = new StackPanel
        {
            Margin = new Thickness(16),
            Spacing = 12,
            Children = { title, _filters, _error, _empty, new ScrollViewer { Content = _rows } },
        };
        model.Rows.CollectionChanged += (_, _) => Rebuild();
        model.PropertyChanged += (_, _) => Rebuild();
        Rebuild();
    }

    private void Rebuild()
    {
        _rows.Children.Clear();
        foreach (var row in _model.Rows)
        {
            var line = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 12 };
            line.Children.Add(new TextBlock { Text = row.Reference, FontWeight = FontWeight.SemiBold });
            line.Children.Add(new TextBlock { Text = row.Summary.Title });
            if (row.Waiting is not null)
            {
                line.Children.Add(new TextBlock { Text = row.Waiting, Opacity = 0.7 });
            }

            if (row.Badge is not null)
            {
                line.Children.Add(new Border
                {
                    Padding = new Thickness(6, 1),
                    CornerRadius = new CornerRadius(8),
                    BorderThickness = new Thickness(1),
                    BorderBrush = row.Urgency == Urgency.Overdue ? Brushes.IndianRed : Brushes.Goldenrod,
                    Child = new TextBlock { Text = row.Badge },
                });
            }

            AutomationProperties.SetName(line, row.Reference + " " + row.Summary.Title + (row.Badge is null ? string.Empty : ", " + row.Badge));
            _rows.Children.Add(line);
        }

        _empty.IsVisible = _model.IsEmpty;
        _error.IsVisible = _model.Error is not null;
        _error.Text = _model.Error;
    }
}

/// <summary>The request page: timeline, records, tasks, comments, conversations, Undo and Cancel.</summary>
public sealed class RequestPageView : UserControl
{
    /// <summary>Creates the page over a loaded <paramref name="model"/>.</summary>
    public RequestPageView(RequestPageViewModel model)
    {
        ArgumentNullException.ThrowIfNull(model);
        FlowDirection = model.Text.FlowDirection;
        var text = model.Text;
        var panel = new StackPanel { Margin = new Thickness(16), Spacing = 12 };
        panel.Children.Add(new TextBlock { Text = model.Title, FontSize = 20, FontWeight = FontWeight.SemiBold });
        panel.Children.Add(new TextBlock { Text = model.StateText, Opacity = 0.7 });
        AutomationProperties.SetName(this, model.Title);
        if (model.Error is not null)
        {
            panel.Children.Add(new TextBlock { Text = model.Error });
        }

        Section(panel, text.Get("page.timeline"), model.Timeline.Select(t =>
            $"{t.StateText}: {t.Step.Title}{(t.Step.Person is null ? string.Empty : " - " + t.Step.Person)}"));
        if (model.Detail is { } detail)
        {
            Section(panel, text.Get("page.records"), detail.Records.Select(r => $"{r.Title} ({r.Id})"));
            Section(panel, text.Get("page.tasks"), detail.Tasks.Select(t => t.Title + (t.Assignee is null ? string.Empty : " - " + t.Assignee)));
            Section(panel, text.Get("page.comments"), detail.Comments.Select(c => $"{c.Author}: {c.Text}"));
        }

        if (model.Conversations.Count > 0)
        {
            panel.Children.Add(new TextBlock { Text = text.Get("page.conversations"), FontWeight = FontWeight.SemiBold });
            foreach (var conversation in model.Conversations)
            {
                panel.Children.Add(Conversation(conversation));
            }
        }

        var actions = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8 };
        foreach (var (action, run) in new[] { (model.Undo, new Func<Task>(() => model.UndoAsync())), (model.Cancel, new Func<Task>(() => model.CancelAsync(null))) })
        {
            if (action is null)
            {
                continue;
            }

            var button = new Button { Content = action.Label, IsEnabled = action.IsEnabled };
            if (action.DisabledReason is not null)
            {
                ToolTip.SetTip(button, action.DisabledReason);
                AutomationProperties.SetHelpText(button, action.DisabledReason);
            }

            // Undo and Cancel change the run: the first press asks, the second confirms.
            var armed = false;
            var label = action.Label;
            button.Click += async (_, _) =>
            {
                if (!armed)
                {
                    armed = true;
                    button.Content = text.Get("page.confirm", ("action", label));
                    return;
                }

                armed = false;
                button.Content = label;
                await run().ConfigureAwait(true);
            };
            button.LostFocus += (_, _) =>
            {
                armed = false;
                button.Content = label;
            };
            actions.Children.Add(button);
        }

        panel.Children.Add(actions);
        foreach (var action in new[] { model.Undo, model.Cancel })
        {
            if (action is { Blockers.Count: > 0 })
            {
                panel.Children.Add(new TextBlock { Text = text.Get("page.blockers"), FontWeight = FontWeight.SemiBold });
                foreach (var blocker in action.Blockers)
                {
                    panel.Children.Add(new TextBlock { Text = blocker });
                }
            }
        }

        Content = new ScrollViewer { Content = panel };
    }

    private static void Section(StackPanel panel, string heading, IEnumerable<string> lines)
    {
        var list = lines.ToArray();
        if (list.Length == 0)
        {
            return;
        }

        panel.Children.Add(new TextBlock { Text = heading, FontWeight = FontWeight.SemiBold });
        foreach (var line in list)
        {
            panel.Children.Add(new TextBlock { Text = line });
        }
    }

    private static Border Conversation(ConversationPanel conversation)
    {
        var body = new StackPanel { Spacing = 4 };
        body.Children.Add(new TextBlock { Text = conversation.Peer, FontWeight = FontWeight.SemiBold });
        if (conversation.Condition is not null)
        {
            body.Children.Add(new TextBlock { Text = conversation.Condition, Foreground = Brushes.IndianRed, TextWrapping = TextWrapping.Wrap });
        }

        foreach (var line in conversation.Lines)
        {
            body.Children.Add(new TextBlock
            {
                Text = line.DeliveryText is null ? line.Message.Text : $"{line.Message.Text} ({line.DeliveryText})",
                TextWrapping = TextWrapping.Wrap,
            });
        }

        return new Border { BorderThickness = new Thickness(1), BorderBrush = Brushes.Gray, Padding = new Thickness(8), Child = body };
    }
}

/// <summary>The form-step screen: step title, instructions, who is waiting, the generic form, and
/// the Save and Complete buttons.</summary>
public sealed class FormStepView : UserControl, IDisposable
{
    private readonly DocumentFormView? _form;

    /// <summary>Creates the screen over a loaded step.</summary>
    public FormStepView(FormStepViewModel model)
    {
        ArgumentNullException.ThrowIfNull(model);
        FlowDirection = model.Text.FlowDirection;
        var root = new DockPanel { Margin = new Thickness(16) };
        var header = new StackPanel { Spacing = 4 };
        header.Children.Add(new TextBlock { Text = model.Title, FontSize = 20, FontWeight = FontWeight.SemiBold });
        if (model.Instructions is not null)
        {
            header.Children.Add(new TextBlock { Text = model.Instructions, TextWrapping = TextWrapping.Wrap });
        }

        if (model.WaitingNotice is not null)
        {
            header.Children.Add(new TextBlock { Text = model.WaitingNotice, Opacity = 0.7 });
        }

        if (model.Status is not null)
        {
            header.Children.Add(new TextBlock { Text = model.Status });
        }

        DockPanel.SetDock(header, Dock.Top);
        root.Children.Add(header);

        var save = new Button { Content = model.SaveLabel, HotKey = KeyGesture.Parse("Ctrl+S") };
        save.Click += async (_, _) => await model.SaveAsync().ConfigureAwait(true);
        var complete = new Button { Content = model.CompleteLabel, IsEnabled = model.CanComplete, HotKey = KeyGesture.Parse("Ctrl+Enter") };
        complete.Click += async (_, _) => await model.CompleteAsync().ConfigureAwait(true);
        var buttons = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 8, HorizontalAlignment = HorizontalAlignment.Right, Children = { save, complete } };
        DockPanel.SetDock(buttons, Dock.Bottom);
        root.Children.Add(buttons);

        if (model.Form is { } form)
        {
            _form = new DocumentFormView(form);
            root.Children.Add(_form);
        }

        AutomationProperties.SetName(this, model.Title);
        Content = root;
    }

    /// <inheritdoc />
    public void Dispose() => _form?.Dispose();
}
