using Avalonia;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media;
using Avalonia.Media.Imaging;
using Avalonia.Styling;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Desktop.Hosting;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Shell;
using Spotledger.Client.Shell.Views;
using Spotledger.Client.Workspace;
using Xunit;

namespace Spotledger.Client.Shell.Tests.Wiring;

/// <summary>The wiring through the real shell view in a real window: the keys, the focus rule, the menu and the pictures.</summary>
public sealed class WindowWiringTests
{
    private static void Pump()
    {
        for (var round = 0; round < 3; round++)
        {
            Dispatcher.UIThread.RunJobs();
            AvaloniaHeadlessPlatform.ForceRenderTimerTick();
            Dispatcher.UIThread.RunJobs();
        }
    }

    private static async Task<(Window Window, ShellViewHooks Hooks, DocumentFormViewModel Form, DocumentFormView View)> ShowAsync(Rig rig)
    {
        var shellView = new ShellView { DataContext = rig.Shell };
        var hooks = new ShellViewHooks(shellView, rig.Shell);
        var window = new Window { Width = 1280, Height = 800, Content = shellView };
        window.Show();
        var form = await rig.OpenAsync();
        Pump();
        var view = Assert.IsType<DocumentFormView>(hooks.ViewOf(rig.Shell.SelectedTab!));
        Pump();
        return (window, hooks, form, view);
    }

    [AvaloniaFact]
    public async Task CtrlZAndCtrlYAndCtrlShiftZWorkInTheWindowButCtrlZStaysTheTextBoxsOwnWhileItHasFocus()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var (window, hooks, form, view) = await ShowAsync(rig);
        using var hookScope = hooks;
        form.Draft!.Set("qty", new DecimalFieldValue(5m));
        Assert.Equal("Undo Edit Quantity", rig.Shell.Commands[WorkspaceCommands.UndoId].Label);

        _ = view.Focus();
        Pump();
        window.KeyPressQwerty(PhysicalKey.Z, RawInputModifiers.Control);
        Assert.Equal(1.00m, ((DecimalFieldValue)form.Draft.Get("qty")!).ExactValue.AsDecimal);
        window.KeyPressQwerty(PhysicalKey.Y, RawInputModifiers.Control);
        Assert.Equal(5m, ((DecimalFieldValue)form.Draft.Get("qty")!).ExactValue.AsDecimal);
        window.KeyPressQwerty(PhysicalKey.Z, RawInputModifiers.Control);
        window.KeyPressQwerty(PhysicalKey.Z, RawInputModifiers.Control | RawInputModifiers.Shift);
        Assert.Equal(5m, ((DecimalFieldValue)form.Draft.Get("qty")!).ExactValue.AsDecimal);

        // With a text box focused, the shell does not take Ctrl+Z: the box has it until its focus ends.
        Pump();
        window.UpdateLayout();
        Pump();
        var box = Assert.IsAssignableFrom<TextBox>(view.ControlOf("qty"));
        _ = box.Focus();
        Pump();
        Assert.True(rig.Shell.IsFocusedText, $"focus={TopLevel.GetTopLevel(view)?.FocusManager?.GetFocusedElement()?.GetType().Name} IsFocused={box.IsFocused} inTree={TopLevel.GetTopLevel(box) is not null} desc={string.Join(",", view.GetVisualDescendants().Select(d => d.GetType().Name).Distinct())} phase={form.Phase} bodyOfView={view.Content?.GetType().Name}");
        Assert.False(rig.Shell.Dispatcher.GetAvailability(WorkspaceCommands.UndoId, rig.Shell.CurrentContext()).IsAvailable);
        window.KeyPressQwerty(PhysicalKey.Z, RawInputModifiers.Control);
        Assert.Equal(5m, ((DecimalFieldValue)form.Draft.Get("qty")!).ExactValue.AsDecimal);

        _ = view.Focus();
        Pump();
        Assert.False(rig.Shell.IsFocusedText);
        window.Close();
    }

    [AvaloniaFact]
    public async Task ChildTableRowUndoAndTextBoxEditsShareOneHistoryThroughTheView()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var (window, hooks, form, view) = await ShowAsync(rig);
        using var hookScope = hooks;
        var table = form.Draft!.Tables["items"];

        form.Draft.Set("title", new TextFieldValue("Changed"));
        _ = form.History!.CanUndo; // closes the first user edit
        _ = table.AddRow();
        Assert.Equal(3, table.Rows.Count);

        Assert.True(form.History.Undo().Succeeded, "the row step is undone through the table gate the view supplied");
        Assert.Equal(2, table.Rows.Count);
        Assert.True(form.History.Undo().Succeeded);
        Assert.Equal("October", ((TextFieldValue)form.Draft.Get("title")!).Value);
        Assert.NotNull(view.TableOf("items"));
        window.Close();
    }

    [AvaloniaFact]
    public async Task RowUndoIsRefusedWhileTheTableIsNotEditable()
    {
        using var rig = new Rig(new WiringGateway { Rights = DocumentRights.All with { Write = false } });
        await rig.SignInAsync();
        var form = await rig.OpenAsync();
        var table = form.Draft!.Tables["items"];
        _ = table.AddRow();

        // No view has supplied the table's gate, and the user cannot write: the step stays.
        var result = form.History!.Undo();
        Assert.False(result.Succeeded);
        Assert.Equal(3, table.Rows.Count);
    }

    [AvaloniaFact]
    public async Task EveryEditMenuAndRecoveryScreenshotIsARealPicture()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var seed = await rig.OpenAsync();
        seed.Draft!.Set("title", new TextFieldValue("Draft kept from before"));
        rig.Clock.Advance(TimeSpan.FromSeconds(3));
        await TestHarness.WaitUntilAsync(() => rig.DraftFiles.Any(), "a draft file");
        rig.Shell.CloseTab(rig.Shell.Tabs.Single(t => t.Content is DocumentFormViewModel));
        await TestHarness.WaitUntilAsync(() => rig.Shell.Tabs.All(t => t.Content is not DocumentFormViewModel), "the tab to close");
        await Task.Delay(50);

        var directory = Path.Combine(AppContext.BaseDirectory, "screenshots", "wiring");
        _ = Directory.CreateDirectory(directory);
        var app = Application.Current!;
        var previous = app.RequestedThemeVariant;
        var (window, hooks, form, _) = await ShowAsync(rig);
        using var hookScope = hooks;
        form.Draft!.Set("qty", new DecimalFieldValue(7m));
        Assert.IsType<RecoveryBarView>(form.Recovery);
        foreach (var (name, dark, flow) in new[]
                 {
                     ("light-ltr", false, FlowDirection.LeftToRight),
                     ("dark-ltr", true, FlowDirection.LeftToRight),
                     ("light-rtl", false, FlowDirection.RightToLeft),
                 })
        {
            app.RequestedThemeVariant = dark ? ThemeVariant.Dark : ThemeVariant.Light;
            window.RequestedThemeVariant = app.RequestedThemeVariant;
            window.FlowDirection = flow;
            Pump();
            Assert.Equal("Undo Edit Quantity", rig.Shell.Commands[WorkspaceCommands.UndoId].Label);
            var bitmap = window.CaptureRenderedFrame();
            Assert.NotNull(bitmap);
            bitmap.Save(Path.Combine(directory, $"form-with-recovery-bar-{name}.png"), new PngBitmapEncoderOptions());
            Assert.True(Distinct(bitmap) > 8, "the picture holds more than a blank surface");
        }

        // The Edit menu with the labelled Undo item open.
        window.FlowDirection = FlowDirection.LeftToRight;
        var edit = window.GetVisualDescendants().OfType<MenuItem>().First(item => item.Name == "Menu_Edit");
        edit.IsSubMenuOpen = true;
        Pump();
        var menuShot = window.CaptureRenderedFrame();
        Assert.NotNull(menuShot);
        menuShot.Save(Path.Combine(directory, "edit-menu-with-undo-label.png"), new PngBitmapEncoderOptions());
        var undoItem = window.GetVisualDescendants().OfType<MenuItem>().FirstOrDefault(item => item.Name == "MenuItem_edit_undo");
        Assert.NotNull(undoItem);
        Assert.Contains("Undo Edit Quantity", undoItem.Header?.ToString()?.Replace("_", string.Empty, StringComparison.Ordinal), StringComparison.Ordinal);
        app.RequestedThemeVariant = previous;
        window.Close();
    }

    private static int Distinct(WriteableBitmap bitmap)
    {
        using var framebuffer = bitmap.Lock();
        var seen = new HashSet<int>();
        var stride = framebuffer.RowBytes;
        for (var y = 0; y < framebuffer.Size.Height; y += 5)
        {
            for (var x = 0; x < framebuffer.Size.Width; x += 5)
            {
                seen.Add(System.Runtime.InteropServices.Marshal.ReadInt32(framebuffer.Address, (y * stride) + (x * 4)));
            }
        }

        return seen.Count;
    }

    [AvaloniaFact]
    public async Task AWipeThatLeftDraftsBehindIsShownAndRetriedAtTheNextStart()
    {
        using var rig = new Rig();
        await rig.SignInAsync();
        var scope = rig.Drafts.Scope!;
        var form = await rig.OpenAsync();
        form.Draft!.Set("title", new TextFieldValue("Stays"));
        rig.Clock.Advance(TimeSpan.FromSeconds(3));
        await TestHarness.WaitUntilAsync(() => rig.DraftFiles.Any(), "a draft file");

        // A store whose delete does nothing: the wipe fails, is announced and remembered.
        var stubborn = new StubbornStore(new FileDraftStore(new FileDraftStoreOptions { Root = rig.Root }, rig.Secrets));
        var notices = new List<string>();
        var pending = Path.Combine(rig.Root, "pending-wipe");
        using var session = new DraftSessionHost(_ => stubborn, pending);
        session.Notice += notices.Add;
        await session.SwitchAsync(scope);
        await session.SignOutAsync();
        Assert.Contains(notices, n => n.Contains("could not be removed", StringComparison.Ordinal));
        Assert.Single(Directory.GetFiles(pending, "*.wipe"));

        stubborn.Works = true;
        using var next = new DraftSessionHost(_ => stubborn, pending);
        await next.RetryPendingWipesAsync();
        Assert.Empty(Directory.GetFiles(pending, "*.wipe"));
        Assert.Empty(rig.DraftFiles);
    }

    private sealed class StubbornStore(IDraftStore inner) : IDraftStore, IDisposable
    {
        public bool Works { get; set; }

        public string? MemoryOnlyNotice => inner.MemoryOnlyNotice;

        public IReadOnlyList<QuarantineNote> Quarantined => inner.Quarantined;

        public event EventHandler<QuarantineNote>? DraftQuarantined
        {
            add => inner.DraftQuarantined += value;
            remove => inner.DraftQuarantined -= value;
        }

        public Task<bool> IsPersistentAsync(DraftScope scope, CancellationToken cancellationToken = default) => inner.IsPersistentAsync(scope, cancellationToken);

        public Task<DraftSaveResult> SaveAsync(DraftRecord record, CancellationToken cancellationToken = default) => inner.SaveAsync(record, cancellationToken);

        public Task<DraftRecord?> LoadAsync(DraftKey key, CancellationToken cancellationToken = default) => inner.LoadAsync(key, cancellationToken);

        public Task DeleteAsync(DraftKey key, CancellationToken cancellationToken = default) => inner.DeleteAsync(key, cancellationToken);

        public Task<IReadOnlyList<DraftSummary>> ListAsync(DraftScope scope, CancellationToken cancellationToken = default) => inner.ListAsync(scope, cancellationToken);

        public Task DeleteAllAsync(DraftScope scope, CancellationToken cancellationToken = default)
            => Works ? inner.DeleteAllAsync(scope, cancellationToken) : Task.CompletedTask;

        public void Dispose()
        {
        }
    }
}
