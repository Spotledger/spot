using Avalonia;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Layout;
using Avalonia.VisualTree;
using Avalonia.Media;
using AvaloniaGrid = Avalonia.Controls.Grid;
using Spotledger.Client.Controls;
using Spotledger.Client.Forms;
using Spotledger.Client.Lists;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Spotledger.Client.Workspace.Keyboard;

namespace Spotledger.Client.Workspace.Split;

/// <summary>
/// The list and the form side by side, worked without a pointer (WP-UI-17.T2).
/// <para>The list on the left (or on the right in a right-to-left layout, where the grid mirrors itself
/// and the form takes the start edge), the form of the selected row on the right, and a splitter that
/// drags and also resizes from the keyboard. Three modes (list only, split, form only) are commands and
/// are remembered per DocType per user. A selection change never closes a dirty form silently: the
/// change waits behind the unsaved-changes guard. <c>Enter</c>/<c>F2</c> walks into the form, <c>Esc</c>
/// comes back to the row, <c>Alt+Down</c>/<c>Alt+Up</c> walk the list from inside the form, and
/// <c>Ctrl+Enter</c> moves the form to its own tab and keeps the list.</para>
/// <para>Every focus move goes through <see cref="FocusRules"/> and is announced through the live
/// region, so the keyboard-only harness can assert where focus went by reading
/// <see cref="FocusTrace"/>.</para>
/// </summary>
public sealed partial class ListFormSplit : UserControl, IDisposable
{
    /// <summary>The automation id of the list pane.</summary>
    public const string ListPaneId = FocusRules.List;

    /// <summary>The automation id of the form pane.</summary>
    public const string FormPaneId = FocusRules.Form;

    /// <summary>The automation id of the splitter.</summary>
    public const string SplitterId = FocusRules.Splitter;

    /// <summary>The automation id of the unsaved-changes guard.</summary>
    public const string GuardId = FocusRules.Guard;

    private readonly ListView _list;
    private readonly SplitDocumentHost _documents;
    private readonly ISplitPreferenceStore _preferences;
    private readonly string _user;
    private readonly KeymapProfile _keymap;
    private readonly Announcer _announcer = new();
    private readonly AvaloniaGrid _root = new();
    private readonly Border _listPane;
    private readonly Border _formPane;
    private readonly GridSplitter _splitter;
    private readonly ContentControl _formContent = new();
    private readonly Border _guardBar;
    private readonly TextBlock _modeText = new() { Opacity = 0.75 };
    private readonly SplitSelectionGuard _guard = new();
    private readonly HashSet<DocumentFormView> _watched = [];
    private readonly Dictionary<DocumentFormViewModel, bool> _dirtyStates = [];
    private SplitMode _mode;
    private int _listPercent;
    private string? _currentId;
    private bool _walking;
    private bool _disposed;

    /// <summary>Creates the arrangement over a loaded (or loading) list.</summary>
    /// <param name="list">The list view; its <see cref="ListViewModel.SelectedRowChanged"/> drives the form pane.</param>
    /// <param name="documents">The documents open in this arrangement (one form per document id).</param>
    /// <param name="preferences">Where the mode and the splitter share are remembered.</param>
    /// <param name="user">The person the preferences belong to.</param>
    /// <param name="keymap">The extended keymap profile; null loads the desktop profile with the workspace additions.</param>
    /// <param name="trace">The focus trace to write to (tests attach one; a host may leave it null).</param>
    public ListFormSplit(
        ListView list,
        SplitDocumentHost documents,
        ISplitPreferenceStore preferences,
        string user,
        KeymapProfile? keymap = null,
        FocusTrace? trace = null)
    {
        _list = list ?? throw new ArgumentNullException(nameof(list));
        _documents = documents ?? throw new ArgumentNullException(nameof(documents));
        _preferences = preferences ?? throw new ArgumentNullException(nameof(preferences));
        _user = user ?? string.Empty;
        _keymap = keymap ?? WorkspaceKeymaps.Load("desktop");
        Trace = trace;

        var saved = _preferences.Load(_user, _list.Model.DocType) ?? SplitPreference.Default;
        _mode = saved.Mode;
        _listPercent = saved.ListPercent;

        AutomationProperties.SetAutomationId(this, "split");
        AutomationProperties.SetName(this, _list.Model.Title + " and its documents");
        Focusable = false;

        _listPane = new Border { Child = _list, Padding = new Thickness(0) };
        AutomationProperties.SetAutomationId(_listPane, ListPaneId);
        AutomationProperties.SetName(_listPane, _list.Model.Title + " list pane");

        _formPane = new Border { Padding = new Thickness(0) };
        AutomationProperties.SetAutomationId(_formPane, FormPaneId);
        AutomationProperties.SetName(_formPane, "Document form pane");

        _splitter = new GridSplitter
        {
            ResizeDirection = GridResizeDirection.Columns,
            ResizeBehavior = GridResizeBehavior.PreviousAndNext,
            Width = 8,
            Background = Brushes.Transparent,
            Focusable = true,
        };
        AutomationProperties.SetAutomationId(_splitter, SplitterId);
        AutomationProperties.SetName(_splitter, "Resize the form pane");

        _guardBar = BuildGuard();
        var formColumn = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(_guardBar, Dock.Top);
        formColumn.Children.Add(_guardBar);
        formColumn.Children.Add(_formContent);
        _formPane.Child = formColumn;

        var strip = new DockPanel { LastChildFill = true, Margin = new Thickness(0, 4) };
        DockPanel.SetDock(_modeText, Dock.Left);
        AutomationProperties.SetAutomationId(_modeText, "split.mode");
        AutomationProperties.SetName(_modeText, "Arrangement");
        strip.Children.Add(_modeText);
        strip.Children.Add(_announcer);

        _root.ColumnDefinitions.Add(new ColumnDefinition(40, GridUnitType.Star) { MinWidth = 220 });
        _root.ColumnDefinitions.Add(new ColumnDefinition(GridLength.Auto));
        _root.ColumnDefinitions.Add(new ColumnDefinition(60, GridUnitType.Star) { MinWidth = 260 });
        AvaloniaGrid.SetColumn(_formPane, 2);
        _root.Children.Add(_listPane);
        _root.Children.Add(_splitter);
        _root.Children.Add(_formPane);

        var root = new DockPanel { LastChildFill = true };
        DockPanel.SetDock(strip, Dock.Bottom);
        root.Children.Add(strip);
        root.Children.Add(_root);
        Content = root;

        Panes.Add(ListPaneId, _listPane);
        Panes.Add(FormPaneId, _formPane);

        _list.Model.SelectedRowChanged += OnSelectedRowChanged;
        AddHandler(KeyDownEvent, OnKey, RoutingStrategies.Tunnel);
        AttachedToVisualTree += OnFirstAttached;
        ApplyMode(_mode, persist: false);
    }

    /// <summary>The list pane's automation id (a host wires its chrome panes around these two).</summary>
    public string DocType => _list.Model.DocType;

    /// <summary>The focus trace this arrangement writes to, when a test attached one.</summary>
    public FocusTrace? Trace { get; }

    /// <summary>The pane ring, so a host registers the chrome's panes before these two.</summary>
    public PaneRing Panes { get; } = new();

    /// <summary>The list view.</summary>
    public ListView List => _list;

    /// <summary>The documents open in this arrangement.</summary>
    public SplitDocumentHost Documents => _documents;

    /// <summary>The unsaved-changes guard's state.</summary>
    public SplitSelectionGuard Guard => _guard;

    /// <summary>The mode in force.</summary>
    public SplitMode Mode => _mode;

    /// <summary>The list's share of the width, in whole percent.</summary>
    public int ListPercent => _listPercent;

    /// <summary>The document the form pane shows (null: a new document or nothing).</summary>
    public string? CurrentDocumentId => _currentId;

    /// <summary>The form view in the pane, when one is shown.</summary>
    public DocumentFormView? Form => _formContent.Content as DocumentFormView;

    /// <summary>True while the unsaved-changes guard is on screen.</summary>
    public bool IsGuardShown => _guardBar.IsVisible;

    /// <summary>The last asynchronous operation a key started; a test awaits it instead of sleeping.</summary>
    public Task LastOperation { get; private set; } = Task.CompletedTask;

    /// <summary>Raised when the arrangement asks the window to move to the next (1) or previous (-1) tab.</summary>
    public event EventHandler<int>? TabCycleRequested;

    /// <summary>Raised after a document was handed to its own tab.</summary>
    public event EventHandler<string>? OpenedInTab;

    /// <summary>Switches the arrangement and remembers the choice for this DocType and person.</summary>
    public void SetMode(SplitMode mode)
    {
        if (_mode == mode)
        {
            return;
        }

        ApplyMode(mode, persist: true);
        Announce(mode switch
        {
            SplitMode.List => "List only",
            SplitMode.Form => "Form only",
            _ => "List and form",
        });
    }

    /// <summary>Moves the splitter by <paramref name="delta"/> whole percent and remembers it.</summary>
    public void Nudge(int delta)
    {
        var next = Math.Clamp(_listPercent + delta, SplitPreference.MinListPercent, SplitPreference.MaxListPercent);
        if (next == _listPercent)
        {
            Announce(_listPercent >= SplitPreference.MaxListPercent ? "The list is as wide as it goes" : "The list is as narrow as it goes");
            return;
        }

        _listPercent = next;
        ApplyColumns();
        SavePreference();
        Announce($"The list takes {_listPercent} percent");
    }

    /// <summary>Opens <paramref name="id"/> (null: a new document) in the form pane.</summary>
    public Task ShowAsync(string? id) => LastOperation = ShowCoreAsync(id);

    /// <summary>Selects the document at <paramref name="id"/> in the list, if it is on screen.</summary>
    public bool SelectRow(string? id)
    {
        if (id is null)
        {
            return false;
        }

        var grid = _list.Grid;
        for (var row = 0; row < grid.RowCount; row++)
        {
            if (grid.TryGetRow(row, out var value) && string.Equals(value.Key as string, id, StringComparison.Ordinal))
            {
                grid.SetSelection([row]);
                grid.ScrollIntoView(row);
                return true;
            }
        }

        return false;
    }

    /// <summary>Selects the next (<paramref name="step"/> = 1) or previous (-1) row and shows its form.</summary>
    public Task WalkAsync(int step) => LastOperation = WalkCoreAsync(step);

    /// <summary>Saves the form; false when the server refused (the first invalid field then has the focus).</summary>
    public Task<bool> SaveAsync() => SaveCoreAsync();

    /// <summary>Answers the unsaved-changes guard.</summary>
    public Task AnswerGuardAsync(GuardChoice choice) => LastOperation = AnswerGuardCoreAsync(choice);

    /// <summary>Undoes the form's last step (Ctrl+Z outside a text field).</summary>
    public bool Undo() => _formContent.Content is DocumentFormView view && (view.Model.History?.Undo().Succeeded ?? false);

    /// <summary>Redoes the form's last undone step.</summary>
    public bool Redo() => _formContent.Content is DocumentFormView view && (view.Model.History?.Redo().Succeeded ?? false);

    /// <summary>Moves focus into the form, on its first field that can take it.</summary>
    public bool FocusForm() => TryFocusFirstField() || FocusPane(FormPaneId, FocusRule.AfterPaneSwitch);

    private bool TryFocusFirstField()
    {
        if (_formContent.Content is DocumentFormView view && view.Model.Layout is { } layout)
        {
            foreach (var tab in layout.Tabs)
            {
                foreach (var section in tab.Sections)
                {
                    foreach (var field in section.Fields)
                    {
                        if (ControlFor(view, FocusTrace.FieldPrefix + field.Name) is { } control
                            && EntryOf(control) is { } entry
                            && entry.Focus())
                        {
                            Trace?.Rule(FocusRule.AfterOpen.ToString(), FocusTrace.FieldPrefix + field.Name);
                            return true;
                        }
                    }
                }
            }
        }

        return FocusPane(FormPaneId, FocusRule.AfterPaneSwitch);
    }

    /// <summary>Moves focus into the list, on the selected row.</summary>
    public bool FocusListPane() => FocusPane(ListPaneId, FocusRule.AfterPaneSwitch);

    /// <summary>
    /// Puts the focus on the form's first field, now if the layout has already realized it, otherwise at
    /// the first layout pass that does (a form that has just been loaded has no controls to focus yet,
    /// and a sleep is not a way to wait for one).
    /// </summary>
    public void RequestFormFocus()
    {
        RealizeForm();
        if (TryFocusFirstField())
        {
            return;
        }

        _ = FocusPane(FormPaneId, FocusRule.AfterOpen);
        if (_formContent.Content is not DocumentFormView view)
        {
            return;
        }

        void OnLayout(object? sender, EventArgs e)
        {
            if (TryFocusFirstField())
            {
                view.LayoutUpdated -= OnLayout;
            }
        }

        view.LayoutUpdated += OnLayout;
    }

    /// <summary>
    /// Lays the form out once before the focus is placed in it. A form that was just loaded has no
    /// realized editor to focus (its field controls come from their templates), and waiting for a render
    /// tick would mean sleeping in a test. Measuring the pane's content realizes it now; the layout
    /// system measures the pane again on the next pass and reaches the same result.
    /// </summary>
    private void RealizeForm()
    {
        if (_formContent.Content is not Control view || view.Bounds.Width > 0)
        {
            return;
        }

        var width = _formPane.Bounds.Width > 0 ? _formPane.Bounds.Width : 640;
        var height = _formPane.Bounds.Height > 0 ? _formPane.Bounds.Height : 480;
        view.Measure(new Size(width, height));
        view.Arrange(new Rect(0, 0, width, height));
    }

    /// <summary>Cycles the panes (F6 = 1, Shift+F6 = -1).</summary>
    public bool CyclePanes(int step)
    {
        var moved = Panes.Cycle(step);
        if (moved)
        {
            var pane = Panes.FocusedPane;

            // Entering the form pane means entering its first field, not its first toolbar button.
            if (pane == FormPaneId && TryFocusFirstField())
            {
                pane = FormPaneId;
            }

            if (pane is not null)
            {
                Trace?.Rule(FocusRule.AfterPaneSwitch.ToString(), pane);
            }
        }

        return moved;
    }

    /// <inheritdoc />
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        _disposed = true;
        _list.Model.SelectedRowChanged -= OnSelectedRowChanged;
        _documents.Dispose();
        _list.Dispose();
    }

    /// <summary>
    /// The workspace opens with the list focused, so a keyboard-only person is never stranded on
    /// nothing. Only when nothing else in the window took the focus first.
    /// </summary>
    private void OnFirstAttached(object? sender, VisualTreeAttachmentEventArgs e)
    {
        AttachedToVisualTree -= OnFirstAttached;
        if (TopLevel.GetTopLevel(this)?.FocusManager?.GetFocusedElement() is null)
        {
            _ = FocusListPane();
        }
    }

    private void ApplyMode(SplitMode mode, bool persist)
    {
        _mode = mode;
        var listVisible = mode != SplitMode.Form;
        var formVisible = mode != SplitMode.List;
        _listPane.IsVisible = listVisible;
        _formPane.IsVisible = formVisible;
        _splitter.IsVisible = mode == SplitMode.Split;
        _modeText.Text = mode switch
        {
            SplitMode.List => "List only",
            SplitMode.Form => "Form only",
            _ => "List and form",
        };
        ApplyColumns();
        if (persist)
        {
            SavePreference();
        }
    }

    private void ApplyColumns()
    {
        switch (_mode)
        {
            case SplitMode.List:
                _root.ColumnDefinitions[0].Width = new GridLength(1, GridUnitType.Star);
                _root.ColumnDefinitions[1].Width = new GridLength(0);
                _root.ColumnDefinitions[2].Width = new GridLength(0);
                break;
            case SplitMode.Form:
                _root.ColumnDefinitions[0].Width = new GridLength(0);
                _root.ColumnDefinitions[1].Width = new GridLength(0);
                _root.ColumnDefinitions[2].Width = new GridLength(1, GridUnitType.Star);
                break;
            default:
                _root.ColumnDefinitions[0].Width = new GridLength(_listPercent, GridUnitType.Star);
                _root.ColumnDefinitions[1].Width = GridLength.Auto;
                _root.ColumnDefinitions[2].Width = new GridLength(100 - _listPercent, GridUnitType.Star);
                break;
        }
    }

    private void SavePreference() => _preferences.Save(_user, _list.Model.DocType, new SplitPreference(_mode, _listPercent));

    private async Task ShowCoreAsync(string? id)
    {
        if (id is not null && _documents.IsInTab(id))
        {
            _formContent.Content = null;
            _currentId = id;
            Announce("This document is open in its own tab; close the tab to show it here");
            FocusListPane();
            return;
        }

        var view = _documents.View(id);
        _formContent.Content = view;
        WatchFields(view);
        if (view.Model.Phase == FormPhase.Loading)
        {
            await view.Model.LoadAsync().ConfigureAwait(true);
        }

        TagFields(view);

        _currentId = id;
        if (id is not null)
        {
            _ = SelectRow(id);
        }

        if (view.Model.Phase == FormPhase.Failed)
        {
            Announce(view.Model.LoadError ?? "The document could not be loaded");
            FocusPane(FormPaneId, FocusRule.AfterError);
            return;
        }

        Announce(id is null ? "New document" : "Document " + id);
        RequestFormFocus();
    }

    private async Task WalkCoreAsync(int step)
    {
        if (_guard.IsPending)
        {
            Announce("Answer the unsaved-changes question first");
            return;
        }

        var grid = _list.Grid;
        if (grid.RowCount <= 0)
        {
            Announce("The list has no rows");
            return;
        }

        var selected = grid.SelectedRows;
        var current = selected.Count > 0 ? selected[0] : -1;
        var next = current < 0 ? 0 : current + step;
        if (next < 0)
        {
            Announce("This is the first row");
            return;
        }

        if (next >= grid.RowCount)
        {
            Announce("This is the last row");
            return;
        }

        var keptFocus = Panes.FocusedPane == FormPaneId;
        var id = grid.TryGetRow(next, out var row) ? row.Key as string : null;
        _walking = true;
        try
        {
            grid.SetSelection([next]);
            grid.ScrollIntoView(next);
        }
        finally
        {
            _walking = false;
        }

        await ShowCoreAsync(id).ConfigureAwait(true);
        Announce($"Row {next + 1} of {grid.RowCount}, {id}");
        if (keptFocus)
        {
            _ = FocusForm();
        }
    }

    private async Task<bool> SaveCoreAsync()
    {
        if (_formContent.Content is not DocumentFormView view)
        {
            Announce("There is no form to save");
            return false;
        }

        var wasDirty = view.Model.IsDirty;
        var ok = await view.Model.SaveAsync().ConfigureAwait(true);
        if (!ok)
        {
            ApplyValidationFailure(view);
            return false;
        }

        if (_currentId is null && view.Model.DocumentId is { } created)
        {
            _documents.Rekey(null, created);
            _currentId = created;
        }

        _guard.Reset();
        HideGuard();
        var target = FocusRules.Resolve(FocusRule.AfterSave, new FocusContext { Previous = Trace?.Current });
        FocusTarget(target);
        if (!wasDirty)
        {
            // Nothing changed, so no dirty transition announced anything: say it here.
            Announce(target.Announcement ?? "Saved");
        }

        return true;
    }

    private async Task AnswerGuardCoreAsync(GuardChoice choice)
    {
        switch (choice)
        {
            case GuardChoice.Save:
                // Save clears the guard, so the row that was waiting is read before the save starts.
                var waiting = _guard.PendingId;
                if (!await SaveCoreAsync().ConfigureAwait(true))
                {
                    return;
                }

                HideGuard();
                await ShowCoreAsync(waiting).ConfigureAwait(true);
                break;

            case GuardChoice.Discard:
                var next = _guard.Commit();
                HideGuard();
                _documents.Settle(_currentId);
                await ShowCoreAsync(next).ConfigureAwait(true);
                break;

            default:
                _guard.Cancel();
                HideGuard();
                _ = SelectRow(_currentId);
                FocusPane(ListPaneId, FocusRule.AfterCancel);
                Announce("Staying on this document");
                break;
        }
    }

    private void OnSelectedRowChanged(object? sender, ListRowSelection selection)
    {
        if (_disposed)
        {
            return;
        }

        if (_walking)
        {
            return;
        }

        if (_formContent.Content is DocumentFormView current && current.Model.IsDirty
            && !_guard.TryChange(_currentId, selection.Id, dirty: true))
        {
            ShowGuard(selection.Id);
            return;
        }

        if (string.Equals(selection.Id, _currentId, StringComparison.Ordinal) && _formContent.Content is not null)
        {
            return;
        }

        _ = ShowAsync(selection.Id);
    }

    private void ShowGuard(string? pending)
    {
        _guardBar.IsVisible = true;
        GuardQuestion.Text = _guard.Question;
        Announce(_guard.Question + (pending is null ? string.Empty : " Next row: " + pending));
        if (PaneRing.Entry(_guardBar) is { } entry)
        {
            _ = entry.Focus();
        }
    }

    private void HideGuard() => _guardBar.IsVisible = false;

    private void ApplyValidationFailure(DocumentFormView view)
    {
        var field = FirstInvalidField(view.Model);
        // The field's own message is the useful one; the document message is the fallback.
        var message = FieldErrorMessage(view.Model, field) ?? FirstErrorMessage(view.Model) ?? "The form could not be saved";
        var target = FocusRules.Resolve(FocusRule.AfterValidationFailure, new FocusContext
        {
            Previous = Trace?.Current,
            InvalidField = field,
            Message = message,
        });
        FocusTarget(target);
        Announce(target.Announcement ?? message);
        Trace?.Rule(FocusRule.AfterValidationFailure.ToString(), target.AutomationId);
    }

    private static string? FirstInvalidField(DocumentFormViewModel model)
    {
        if (model.Draft is not { } draft || model.Layout is not { } layout)
        {
            return null;
        }

        foreach (var tab in layout.Tabs)
        {
            foreach (var section in tab.Sections)
            {
                foreach (var field in section.Fields)
                {
                    if (draft.Fields.TryGetValue(field.Name, out var state) && state.Errors.Count > 0)
                    {
                        return field.Name;
                    }

                    if (field.Kind == FieldKind.Table
                        && draft.Tables.TryGetValue(field.Name, out var table)
                        && table.Rows.Any(row => row.RowErrors.Count > 0))
                    {
                        return field.Name;
                    }
                }
            }
        }

        return null;
    }

    private static string? FieldErrorMessage(DocumentFormViewModel model, string? field)
        => field is not null
           && model.Draft is { } draft
           && draft.Fields.TryGetValue(field, out var state)
           && state.Errors.Count > 0
            ? state.Errors[0].Message
            : null;

    private static string? FirstErrorMessage(DocumentFormViewModel model)
        => model.Messages.FirstOrDefault(message => message.Level == "error")?.Text;

    private bool FocusPane(string paneId, FocusRule rule)
    {
        if (!Panes.Focus(paneId))
        {
            return false;
        }

        var target = FocusRules.Resolve(rule, new FocusContext { Pane = paneId, Previous = Trace?.Current });
        Trace?.Rule(rule.ToString(), target.AutomationId);
        return true;
    }

    private void FocusTarget(FocusTarget target)
    {
        RealizeForm();
        if (target.AutomationId.StartsWith(FocusTrace.FieldPrefix, StringComparison.Ordinal)
            && _formContent.Content is DocumentFormView view
            && ControlFor(view, target.AutomationId) is { } control
            && EntryOf(control) is { } entry)
        {
            _ = entry.Focus();
            if (target.ScrollIntoView)
            {
                entry.BringIntoView();
            }

            return;
        }

        _ = target.AutomationId == FormPaneId ? Panes.Focus(FormPaneId) : Panes.Focus(target.AutomationId);
    }

    /// <summary>
    /// The control that takes the focus for a field. A field control is a wrapper whose text box lives in
    /// its template: the template is applied here, so the focus can be placed the moment the form is laid
    /// out instead of waiting for a render pass.
    /// </summary>
    private static Control? EntryOf(Control control)
    {
        if (control is Avalonia.Controls.Primitives.TemplatedControl templated)
        {
            templated.ApplyTemplate();
        }

        return PaneRing.Entry(control);
    }

    private static Control? ControlFor(DocumentFormView view, string automationId)
    {
        var name = automationId[FocusTrace.FieldPrefix.Length..];
        var control = view.ControlOf(name);
        if (control is null)
        {
            return null;
        }

        return AutomationProperties.GetAutomationId(control) is { Length: > 0 } ? control : view.WrapperOf(name) as Control ?? control;
    }

    private void WatchFields(DocumentFormView view)
    {
        if (_watched.Add(view))
        {
            var model = view.Model;
            model.LayoutChanged += (_, _) => TagFields(view);
            _dirtyStates[model] = model.IsDirty;
            model.PropertyChanged += (_, e) =>
            {
                if (e.PropertyName == nameof(DocumentFormViewModel.IsDirty))
                {
                    OnDirtyChanged(model);
                }
            };
        }

        TagFields(view);
    }

    /// <summary>
    /// A form that became clean was saved (by Ctrl+S in the form or by the arrangement's own save), so the
    /// arrangement says so once, wherever the save came from.
    /// </summary>
    private void OnDirtyChanged(DocumentFormViewModel model)
    {
        var was = _dirtyStates.TryGetValue(model, out var known) && known;
        var now = model.IsDirty;
        _dirtyStates[model] = now;
        if (was && !now)
        {
            var target = FocusRules.Resolve(FocusRule.AfterSave, new FocusContext { Previous = Trace?.Current });
            Announce(target.Announcement ?? "Saved");
        }
    }

    private void TagFields(DocumentFormView view)
    {
        if (view.Model.Layout is not { } layout)
        {
            return;
        }

        foreach (var tab in layout.Tabs)
        {
            foreach (var section in tab.Sections)
            {
                foreach (var field in section.Fields)
                {
                    if (view.ControlOf(field.Name) is { } control)
                    {
                        AutomationProperties.SetAutomationId(control, FocusTrace.FieldPrefix + field.Name);
                        AutomationProperties.SetName(control, field.Label);
                    }
                }
            }
        }
    }
}
