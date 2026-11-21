using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Api;
using Xunit;

namespace Spotledger.Client.Requests.Tests;

public sealed class StartTests
{
    private static EntryInfo Info(EntryMode mode) => new("expense", mode, mode == EntryMode.ProcessOnly ? "proc-expense" : null, "expense claim");

    [Fact]
    public void Start_replaces_New_for_a_process_only_type()
    {
        var action = StartPolicy.CreateActionFor(Info(EntryMode.ProcessOnly), Texts.English());

        Assert.Equal(CreateKind.Start, action.Kind);
        Assert.Equal("Start expense claim", action.Label);
        Assert.Equal("proc-expense", action.ProcessId);
    }

    [Theory]
    [InlineData(EntryMode.Free)]
    [InlineData(EntryMode.Governed)]
    public void Free_and_governed_types_keep_New(EntryMode mode)
    {
        var action = StartPolicy.CreateActionFor(Info(mode), Texts.English());

        Assert.Equal(CreateKind.New, action.Kind);
        Assert.Equal("New", action.Label);
        Assert.Null(action.ProcessId);
    }

    [Fact]
    public void The_keyboard_shortcut_is_the_same_for_New_and_Start()
    {
        var text = Texts.English();

        Assert.Equal(
            StartPolicy.CreateActionFor(Info(EntryMode.Free), text).Gesture,
            StartPolicy.CreateActionFor(Info(EntryMode.ProcessOnly), text).Gesture);
    }

    [Fact]
    public void A_process_only_type_without_a_known_entry_process_still_offers_New_so_the_server_can_route()
    {
        var info = new EntryInfo("expense", EntryMode.ProcessOnly, null, null);

        Assert.Equal(CreateKind.New, StartPolicy.CreateActionFor(info, Texts.English()).Kind);
    }

    [Fact]
    public void The_start_process_refusal_routes_to_Start_with_a_sentence_not_the_raw_error()
    {
        var error = new ApiError
        {
            Code = StartPolicy.StartProcessCode,
            Message = "bpm_entry.start_process: expense must be started",
            FieldErrors = [new ApiFieldError("process", "bpm_entry.start_process", "proc-expense")],
        };

        var route = StartPolicy.RouteFor(error, Info(EntryMode.ProcessOnly), Texts.English());

        Assert.NotNull(route);
        Assert.Equal("proc-expense", route.ProcessId);
        Assert.DoesNotContain("bpm_entry", route.Message, StringComparison.Ordinal);
        Assert.Contains("expense claim", route.Message, StringComparison.Ordinal);
    }

    [Fact]
    public void The_route_falls_back_to_the_known_entry_process()
    {
        var error = new ApiError { Code = StartPolicy.StartProcessCode, Message = "x" };

        Assert.Equal("proc-expense", StartPolicy.RouteFor(error, Info(EntryMode.ProcessOnly), Texts.English())?.ProcessId);
        Assert.Null(StartPolicy.RouteFor(error, null, Texts.English()));
    }

    [Fact]
    public void Another_error_is_not_routed()
    {
        var error = new ApiError { Code = "document.not_found", Message = "x" };

        Assert.Null(StartPolicy.RouteFor(error, Info(EntryMode.ProcessOnly), Texts.English()));
    }

    [AvaloniaFact]
    public void The_create_button_carries_the_label_and_runs_on_click()
    {
        var started = 0;
        var button = CreateButton.Create(StartPolicy.CreateActionFor(Info(EntryMode.ProcessOnly), Texts.English()), () => started++);
        var window = TestHost.Show(button);

        Assert.Equal("Start expense claim", button.Content);
        Assert.Equal(Avalonia.Input.Key.N, button.HotKey?.Key);
        button.RaiseEvent(new Avalonia.Interactivity.RoutedEventArgs(Button.ClickEvent));
        Assert.Equal(1, started);
        window.Close();
    }
}
