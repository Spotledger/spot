using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Interactivity;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.ChildTable;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Forms.Tests;

public sealed class FormViewTests
{
    internal static async Task<(DocumentFormView View, DocumentFormViewModel Form, Window Window)> ShowAsync(
        FakeGateway gateway, string? id, FakeShell? shell = null, double width = 1000, double height = 700, FormView? layout = null)
    {
        var form = Fixtures.Form(gateway, id, shell);
        form.ViewDefinition = layout;
        var view = new DocumentFormView(form);
        var window = TestHost.Show(view, width, height);
        await form.LoadAsync();
        TestHost.Pump();
        return (view, form, window);
    }

    internal static async Task Eventually(Func<bool> condition)
    {
        for (var i = 0; i < 200 && !condition(); i++)
        {
            Dispatcher.UIThread.RunJobs();
            await Task.Delay(10);
        }

        Dispatcher.UIThread.RunJobs();
        Assert.True(condition(), "the condition did not become true");
    }

    private static void Key(Control target, Key key, KeyModifiers modifiers)
        => target.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });

    [AvaloniaFact]
    public async Task TheFormRendersFieldsThroughTheFactoryAndTheChildTableThroughTheEditableTable()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var (view, form, window) = await ShowAsync(gateway, "INV-1");

        var title = Assert.IsType<SpotTextInput>(view.ControlOf("title"));
        Assert.Equal("First invoice", title.Text);
        Assert.True(view.WrapperOf("title")!.IsRequired);
        Assert.Equal("Title", view.WrapperOf("title")!.Label);
        Assert.IsType<SpotLookupBox>(view.ControlOf("customer"));
        Assert.NotNull(((SpotLookupBox)view.ControlOf("customer")!).Source);
        Assert.Contains("lookup invoice.customer", gateway.Calls);
        var table = view.TableOf("items");
        Assert.NotNull(table);
        Assert.Equal(2, table!.Model.Table.Rows.Count);
        Assert.True(table.CanWrite);
        Assert.Equal("Invoice First invoice", form.Title);
        Assert.Contains(view.Toolbar.Children.OfType<SpotButton>(), b => (string?)b.Content == "Save");
        window.Close();
    }

    [AvaloniaFact]
    public async Task CtrlSCommitsTheEditorsAndSavesAndCtrlEnterSubmits()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell();
        var (view, form, window) = await ShowAsync(gateway, "INV-1", shell);
        var title = (SpotTextInput)view.ControlOf("title")!;
        title.Text = "Typed in the box";

        Key(view, Avalonia.Input.Key.S, KeyModifiers.Control);
        await Eventually(() => gateway.Calls.Any(c => c.StartsWith("patch", StringComparison.Ordinal)));

        Assert.Contains("\"title\":\"Typed in the box\"", gateway.Bodies.Single(), StringComparison.Ordinal);
        await Eventually(() => !form.IsDirty && !form.IsBusy);

        Key(view, Avalonia.Input.Key.Enter, KeyModifiers.Control);
        await Eventually(() => gateway.Calls.Any(c => c.StartsWith("Submit", StringComparison.Ordinal)));
        Assert.Contains("confirm: Submit Invoice", shell.Asked);
        await Eventually(() => form.Status == DocumentStatus.Posted);
        window.Close();
    }

    [AvaloniaFact]
    public async Task CtrlNOpensANewDocumentTab()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell();
        var (view, _, window) = await ShowAsync(gateway, "INV-1", shell);

        Key(view, Avalonia.Input.Key.N, KeyModifiers.Control);
        await Eventually(() => shell.Opened.Count == 1);

        Assert.Equal(("invoice", (string?)null), shell.Opened[0]);
        window.Close();
    }

    [AvaloniaFact]
    public async Task AFailedSaveShowsTheErrorBesideTheFieldAndFocusesIt()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var (view, form, window) = await ShowAsync(gateway, "INV-1");
        ((SpotTextInput)view.ControlOf("title")!).Text = "Changed";
        gateway.RejectNextWrite = new Api.ApiError
        {
            StatusCode = 422,
            Code = "document.validation_failed",
            Message = "Invalid",
            FieldErrors = [new Api.ApiFieldError("title", "document.invalid_value", "That title is taken")],
        };

        Assert.False(await form.SaveAsync());
        TestHost.Pump();

        Assert.Equal("That title is taken", view.WrapperOf("title")!.ErrorText);
        window.Close();
    }

    [AvaloniaFact]
    public async Task APostedDocumentRendersWithoutEditAffordances()
    {
        var gateway = new FakeGateway().Seed("INV-1", state: "posted");
        var (view, _, window) = await ShowAsync(gateway, "INV-1");

        Assert.True(((SpotTextInput)view.ControlOf("title")!).IsReadOnly);
        Assert.False(view.TableOf("items")!.CanWrite);
        var save = view.Toolbar.Children.OfType<SpotButton>().Single(b => (string?)b.Content == "Save");
        Assert.False(save.IsEnabled);
        window.Close();
    }

    [AvaloniaFact]
    public async Task AMaskedFieldShowsTheMaskTextAndIsNeverSent()
    {
        var gateway = new FakeGateway { Snapshot = Fixtures.Permissions("""{"masked":["secret_note"]}""") }.Seed("INV-1");
        gateway.Documents["INV-1"]["secret_note"] = "****1234";
        var (view, form, window) = await ShowAsync(gateway, "INV-1");

        Assert.IsType<SpotMaskedField>(view.ControlOf("secret_note"));
        ((SpotTextInput)view.ControlOf("title")!).Text = "Changed";
        Assert.True(await form.SaveAsync());
        Assert.DoesNotContain("secret_note", gateway.Bodies.Single(), StringComparison.Ordinal);
        window.Close();
    }

    [AvaloniaFact]
    public async Task ColumnsCollapseWithTheWidth()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var (wide, _, wideWindow) = await ShowAsync(gateway, "INV-1", width: 1400);
        var wideColumns = wide.ColumnCount;
        wideWindow.Close();

        var (narrow, _, narrowWindow) = await ShowAsync(new FakeGateway().Seed("INV-1"), "INV-1", width: 420);
        var narrowColumns = narrow.ColumnCount;
        narrowWindow.Close();

        Assert.True(wideColumns >= 2, $"wide form uses {wideColumns} column(s)");
        Assert.Equal(1, narrowColumns);
    }

    [AvaloniaFact]
    public async Task AltDigitsSelectTabsAndF6CyclesTheRegions()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var layout = new FormView(
        [
            new FormViewTab("Main", [new FormViewSection("Basics", ["title", "customer"])]),
            new FormViewTab("Lines", [new FormViewSection("Items", ["items"])]),
        ]);
        var (view, _, window) = await ShowAsync(gateway, "INV-1", layout: layout);

        Assert.NotNull(view.Tabs);
        Assert.Equal(2, view.Tabs!.ItemCount);
        Assert.Equal(0, view.Tabs.SelectedIndex);
        Key(view, Avalonia.Input.Key.D2, KeyModifiers.Alt);
        Assert.Equal(1, view.Tabs.SelectedIndex);
        Key(view, Avalonia.Input.Key.D1, KeyModifiers.Alt);
        Assert.Equal(0, view.Tabs.SelectedIndex);

        Key(view, Avalonia.Input.Key.F6, KeyModifiers.None);
        TestHost.Pump();
        Assert.Contains(view.Toolbar.Children.OfType<Control>(), c => c.IsFocused || c.IsKeyboardFocusWithin);
        window.Close();
    }

    [AvaloniaFact]
    public async Task TheConflictPanelOffersReloadAndOverwriteAndNeverSendsOnItsOwn()
    {
        var gateway = new FakeGateway().Seed("INV-1");
        var shell = new FakeShell { Choice = ConflictChoice.Keep };
        var (view, form, window) = await ShowAsync(gateway, "INV-1", shell);
        ((SpotTextInput)view.ControlOf("title")!).Text = "Mine";
        gateway.OtherPersonChanges("INV-1", "Theirs");

        Assert.False(await form.SaveAsync());
        TestHost.Pump();

        Assert.NotNull(form.Conflict);
        var panel = view.GetVisualChildren();
        Assert.Contains(panel.SelectMany(v => v.GetSelfAndVisualDescendants()).OfType<SpotButton>(), b => (string?)b.Content == "Reload and re-apply my changes");
        Assert.Equal("Theirs", gateway.Documents["INV-1"]["title"]!.GetValue<string>());
        window.Close();
    }
}
