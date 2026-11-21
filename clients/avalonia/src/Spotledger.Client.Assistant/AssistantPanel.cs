using System.Collections.Specialized;
using System.ComponentModel;
using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Controls.Documents;
using Avalonia.Input;
using Avalonia.Layout;
using Avalonia.Media;

namespace Spotledger.Client.Assistant;

/// <summary>
/// The docked chat panel, built in code so it takes no markup dependency. Every control
/// has an automation name, the whole panel works from the keyboard (Enter sends, Ctrl+Shift+A
/// toggles) and assistant text is rendered only as text runs: no HTML, no images, and links
/// only for registered destinations.
/// </summary>
public sealed class AssistantPanel : UserControl
{
    private readonly AssistantViewModel _vm;
    private readonly Action<string> _openDestination;
    private readonly StackPanel _transcript = new() { Spacing = 8 };
    private readonly ScrollViewer _scroll;
    private readonly WrapPanel _chips = new();
    private readonly TextBox _input;
    private readonly Button _send;
    private readonly TextBlock _privacy = new() { FontSize = 11, Opacity = 0.8, TextWrapping = TextWrapping.Wrap };
    private readonly Border _banner;
    private readonly TextBlock _bannerText = new() { TextWrapping = TextWrapping.Wrap };

    /// <summary>Creates the panel for <paramref name="vm"/>; <paramref name="openDestination"/> opens a registered link.</summary>
    public AssistantPanel(AssistantViewModel vm, Action<string> openDestination)
    {
        _vm = vm ?? throw new ArgumentNullException(nameof(vm));
        _openDestination = openDestination ?? throw new ArgumentNullException(nameof(openDestination));
        AutomationProperties.SetName(this, "Assistant");
        Focusable = false;

        _scroll = new ScrollViewer { Content = _transcript, VerticalScrollBarVisibility = Avalonia.Controls.Primitives.ScrollBarVisibility.Auto };
        AutomationProperties.SetName(_scroll, "Conversation");
        AutomationProperties.SetName(_transcript, "Assistant transcript");

        _input = new TextBox { AcceptsReturn = false, PlaceholderText = "Ask about a problem", TextWrapping = TextWrapping.Wrap };
        AutomationProperties.SetName(_input, "Message to the assistant");
        _input.KeyBindings.Add(new KeyBinding { Gesture = new KeyGesture(Key.Enter), Command = _vm.SendCommand });
        _input.TextChanged += (_, _) => { if (_vm.Input != (_input.Text ?? string.Empty)) { _vm.Input = _input.Text ?? string.Empty; } };

        _send = new Button { Content = "Send", Command = _vm.SendCommand };
        AutomationProperties.SetName(_send, "Send message");

        _bannerText.Text = string.Empty;
        _banner = new Border { Child = _bannerText, Padding = new Thickness(8), IsVisible = false };
        AutomationProperties.SetName(_banner, "Budget notice");
        AutomationProperties.SetLiveSetting(_banner, AutomationLiveSetting.Assertive);

        var bottom = new StackPanel { Spacing = 4, Children = { _chips, BuildEscalation(), new DockPanel { Children = { DockTo(_send, Avalonia.Controls.Dock.Right), _input } } } };
        var root = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(_privacy, Avalonia.Controls.Dock.Top);
        DockPanel.SetDock(_banner, Avalonia.Controls.Dock.Top);
        DockPanel.SetDock(bottom, Avalonia.Controls.Dock.Bottom);
        root.Children.Add(_privacy);
        root.Children.Add(_banner);
        root.Children.Add(bottom);
        root.Children.Add(_scroll);
        Content = root;

        _vm.PropertyChanged += OnVmChanged;
        _vm.Transcript.CollectionChanged += OnTranscript;
        _vm.Chips.CollectionChanged += (_, _) => RebuildChips();
        Width = _vm.PanelWidth;
        IsVisible = _vm.IsOpen;
        RebuildAll();
    }

    /// <summary>The input box, for focus management and tests.</summary>
    public TextBox InputBox => _input;

    /// <summary>Registers the toggle chord on the host window so it works while the dock is closed or unfocused.</summary>
    public void RegisterGlobalToggle(TopLevel host)
    {
        ArgumentNullException.ThrowIfNull(host);
        if (host is InputElement element)
        {
            element.KeyBindings.Add(new KeyBinding { Gesture = KeyGesture.Parse(AssistantCommands.DefaultChord), Command = _vm.ToggleCommand });
        }
    }

    // "Ask a human": builds the redacted bundle, shows it editable, and sends only when the user presses Send.
    private StackPanel BuildEscalation()
    {
        var ask = new Button { Content = "Ask a human" };
        AutomationProperties.SetName(ask, "Ask a human");
        var bundle = new TextBox { AcceptsReturn = true, TextWrapping = TextWrapping.Wrap, MaxHeight = 160 };
        AutomationProperties.SetName(bundle, "Diagnostic bundle to review");
        var summary = new TextBox { PlaceholderText = "What do you need help with?", MaxLength = 500 };
        AutomationProperties.SetName(summary, "Support request summary");
        var send = new Button { Content = "Send to support" };
        AutomationProperties.SetName(send, "Send the reviewed bundle to support");
        var discard = new Button { Content = "Discard" };
        AutomationProperties.SetName(discard, "Discard the support request");
        var review = new StackPanel { Spacing = 4, IsVisible = false, Children = { new TextBlock { Text = "This is exactly what will be sent. Edit it if you wish.", TextWrapping = TextWrapping.Wrap }, bundle, summary, new StackPanel { Orientation = Orientation.Horizontal, Spacing = 4, Children = { send, discard } } } };
        ask.Click += (_, _) => _vm.PrepareHumanRequest();
        bundle.LostFocus += (_, _) => { if (_vm.PendingBundle is not null && bundle.Text != _vm.PendingBundle.Text) { _vm.EditBundle(bundle.Text ?? string.Empty); } };
        send.Click += async (_, _) =>
        {
            if (_vm.PendingBundle is null) { return; }
            if (bundle.Text != _vm.PendingBundle.Text) { _vm.EditBundle(bundle.Text ?? string.Empty); }
            try { await _vm.SendHumanRequestAsync(summary.Text ?? string.Empty, CancellationToken.None); }
            catch (Exception ex) when (ex is HttpRequestException or OperationCanceledException) { _vm.Transcript.Add(new NoticeEntry("The support request could not be sent. Try again.")); }
        };
        discard.Click += (_, _) => _vm.DiscardHumanRequest();
        _vm.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(AssistantViewModel.PendingBundle))
            {
                review.IsVisible = _vm.PendingBundle is not null;
                bundle.Text = _vm.PendingBundle?.Text ?? string.Empty;
            }
        };
        return new StackPanel { Spacing = 4, Children = { ask, review } };
    }

    private static Control DockTo(Control c, Avalonia.Controls.Dock d)
    {
        DockPanel.SetDock(c, d);
        return c;
    }

    private void OnVmChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(AssistantViewModel.IsOpen): IsVisible = _vm.IsOpen; break;
            case nameof(AssistantViewModel.PanelWidth): Width = _vm.PanelWidth; break;
            case nameof(AssistantViewModel.Input) when (_input.Text ?? string.Empty) != _vm.Input: _input.Text = _vm.Input; break;
            case nameof(AssistantViewModel.PrivacyText): _privacy.Text = _vm.PrivacyText; break;
            case nameof(AssistantViewModel.BudgetBanner):
                _bannerText.Text = _vm.BudgetBanner ?? string.Empty;
                _banner.IsVisible = _vm.BudgetBanner is not null;
                break;
        }
    }

    private void OnTranscript(object? sender, NotifyCollectionChangedEventArgs e) => RebuildAll();

    private void RebuildAll()
    {
        _privacy.Text = _vm.PrivacyText;
        _transcript.Children.Clear();
        foreach (var entry in _vm.Transcript)
        {
            _transcript.Children.Add(Render(entry));
        }

        RebuildChips();
    }

    private void RebuildChips()
    {
        _chips.Children.Clear();
        foreach (var chip in _vm.Chips)
        {
            var remove = new Button { Content = "x", Command = _vm.RemoveChipCommand, CommandParameter = chip };
            AutomationProperties.SetName(remove, "Remove " + chip.Label);
            var row = new StackPanel { Orientation = Orientation.Horizontal, Spacing = 2, Margin = new Thickness(0, 0, 6, 0) };
            row.Children.Add(new TextBlock { Text = chip.Label, VerticalAlignment = VerticalAlignment.Center });
            row.Children.Add(remove);
            _chips.Children.Add(row);
        }
    }

    private StackPanel Render(TranscriptEntry entry)
    {
        Control body = entry switch
        {
            UserEntry u => Plain(u.Text),
            AssistantEntry a => Markdown(a),
            ToolEntry t => Tool(t),
            FactEntry f => Plain(f.Text),
            NoticeEntry n => Plain(n.Text),
            FixEntry fx => Fix(fx.Flow),
            _ => Plain(string.Empty),
        };
        var card = new StackPanel { Spacing = 2 };
        card.Children.Add(new TextBlock { Text = entry.Source, FontWeight = FontWeight.SemiBold, FontSize = 11 });
        card.Children.Add(body);
        AutomationProperties.SetName(card, entry.Source);
        return card;
    }

    private static TextBlock Plain(string text) => new() { Text = text, TextWrapping = TextWrapping.Wrap };

    private StackPanel Markdown(AssistantEntry entry)
    {
        var panel = new StackPanel { Spacing = 2 };
        void Fill()
        {
            panel.Children.Clear();
            foreach (var block in entry.Blocks)
            {
                var tb = new TextBlock { TextWrapping = TextWrapping.Wrap };
                if (block.IsBullet) { tb.Inlines!.Add(new Run("\u2022 ")); }
                foreach (var run in block.Runs)
                {
                    tb.Inlines!.Add(RunFor(run));
                }

                panel.Children.Add(tb);
            }
        }

        Fill();
        entry.PropertyChanged += (_, e) => { if (e.PropertyName == nameof(AssistantEntry.Blocks)) { Fill(); } };
        return panel;
    }

    private Inline RunFor(MarkdownRun run)
    {
        switch (run.Kind)
        {
            case RunKind.Bold: return new Run(run.Text) { FontWeight = FontWeight.Bold };
            case RunKind.Code: return new Run(run.Text) { FontFamily = new FontFamily("monospace") };
            case RunKind.Link:
                var button = new Button { Content = run.Text, Padding = new Thickness(0), Background = Brushes.Transparent };
                var dest = run.Destination!;
                button.Click += (_, _) => _openDestination(dest);
                AutomationProperties.SetName(button, "Open " + run.Text);
                return new InlineUIContainer { Child = button };
            default: return new Run(run.Text);
        }
    }

    private StackPanel Tool(ToolEntry tool)
    {
        var panel = new StackPanel { Spacing = 2 };
        var toggle = new Button { Content = (tool.IsExpanded ? "v " : "> ") + tool.Activity.Summary };
        AutomationProperties.SetName(toggle, tool.Activity.Summary);
        var evidence = new StackPanel { IsVisible = tool.IsExpanded };
        foreach (var e in tool.Activity.Evidence)
        {
            var link = new Button { Content = e.Label, Padding = new Thickness(0), Background = Brushes.Transparent };
            var dest = e.Destination;
            link.Click += (_, _) => { if (_vm.IsRegisteredDestination(dest)) { _openDestination(dest); } };
            AutomationProperties.SetName(link, "Open " + e.Label);
            evidence.Children.Add(link);
        }

        toggle.Click += (_, _) =>
        {
            tool.IsExpanded = !tool.IsExpanded;
            evidence.IsVisible = tool.IsExpanded;
            toggle.Content = (tool.IsExpanded ? "v " : "> ") + tool.Activity.Summary;
        };
        panel.Children.Add(toggle);
        panel.Children.Add(evidence);
        return panel;
    }

    private static StackPanel Fix(FixFlow flow)
    {
        var panel = new StackPanel { Spacing = 4 };
        panel.Children.Add(Plain($"{flow.Plan.Summary} Tier {flow.Plan.Tier}, risk {flow.Plan.Risk}, " + (flow.Plan.Reversible ? "reversible." : "not reversible.")));
        foreach (var step in flow.Plan.Steps)
        {
            panel.Children.Add(Plain($"{step.Index + 1}. {step.Operation} on {step.Target}: {step.Detail}"));
        }

        if (flow.Plan.Tier == FixTier.T2)
        {
            panel.Children.Add(Plain("Affected records: " + string.Join(", ", flow.Plan.Affected)));
            var typed = new TextBox { PlaceholderText = $"Type \"{flow.RequiredPhrase}\" to confirm" };
            AutomationProperties.SetName(typed, "Typed confirmation");
            typed.TextChanged += (_, _) => flow.TypedConfirmation = typed.Text ?? string.Empty;
            panel.Children.Add(typed);
        }

        var apply = new Button { Content = flow.NeedsApproval ? "Request approval" : "Apply fix", IsEnabled = flow.CanApply || flow.NeedsApproval };
        AutomationProperties.SetName(apply, flow.NeedsApproval ? "Request approval" : "Apply fix");
        var status = Plain(string.Empty);
        var undo = new Button { Content = "Undo", IsVisible = false };
        AutomationProperties.SetName(undo, "Undo the change");
        apply.Click += async (_, _) =>
        {
            // An async-void handler must never throw: a stale click is ignored, and the flow itself
            // turns network failures into a message.
            if (flow.NeedsApproval && flow.Phase == FixPhase.Proposed && flow.PlanShown) { await flow.RequestApprovalAsync(CancellationToken.None); }
            else if (flow.CanApply) { await flow.ApplyAsync(CancellationToken.None); }
        };
        undo.Click += async (_, _) =>
        {
            if (flow.Phase == FixPhase.Applied) { await flow.UndoAsync(CancellationToken.None); }
        };
        flow.PropertyChanged += (_, _) =>
        {
            apply.IsEnabled = flow.CanApply || (flow.NeedsApproval && flow.Phase == FixPhase.Proposed);
            undo.IsVisible = flow.CanUndo;
            status.Text = flow.Phase switch
            {
                FixPhase.NeedsApproval => "Needs approval. Task " + flow.ApprovalTaskId,
                FixPhase.Applied => "Verified. " + flow.Message,
                FixPhase.Failed or FixPhase.Undone or FixPhase.UndoExpired => flow.Message,
                _ => string.Empty,
            };
        };
        panel.Children.Add(apply);
        panel.Children.Add(undo);
        panel.Children.Add(status);
        return panel;
    }
}
