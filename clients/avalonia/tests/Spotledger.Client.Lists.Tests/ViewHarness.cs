using Avalonia.Controls;
using Avalonia.Input;
using Avalonia.Threading;
using Xunit;
using Spotledger.Client.Grid;
using Spotledger.Client.Lists;

namespace Spotledger.Client.Lists.Tests;

public sealed record Shown(ListView View, ListViewModel Model, FakeListGateway Gateway, FakeShell Shell, Window Window);

public static class ViewHarness
{
    public static async Task<Shown> ShowAsync(FakeListGateway? gateway = null, ListSourceOptions? source = null, double width = 1100, double height = 640,
        IListExportSink? sink = null, bool settle = true)
    {
        gateway ??= new FakeListGateway(1000);
        var shell = new FakeShell();
        var options = Fixtures.Options(sink) with { Source = source ?? new ListSourceOptions { PageSize = 100 } };
        var model = new ListViewModel(gateway, shell, new InMemoryFilterStore(), "order", "ana", options);
        await model.LoadAsync(TestContext.Current.CancellationToken);
        var view = new ListView(model);
        var window = TestHost.Show(view, width, height);
        if (settle)
        {
            await SettleAsync(view);
        }

        return new Shown(view, model, gateway, shell, window);
    }

    /// <summary>Pumps the UI until the grid holds its first row (or the wait runs out).</summary>
    public static async Task<bool> SettleAsync(ListView view, int rounds = 200)
    {
        for (var i = 0; i < rounds; i++)
        {
            TestHost.Pump();
            if (view.Grid.TryGetRow(0, out _))
            {
                TestHost.Pump();
                return true;
            }

            await Task.Delay(2, TestContext.Current.CancellationToken);
        }

        return false;
    }

    public static void Press(Control target, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => target.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });

    public static string? CellText(SpotGrid grid, int row, int column)
        => grid.TryGetRow(row, out var value) ? value.Cells[column].Text : null;

    /// <summary>Pumps the UI until <paramref name="condition"/> holds (up to <paramref name="ms"/>).</summary>
    public static async Task<bool> WaitAsync(Func<bool> condition, int ms = 3000)
    {
        for (var waited = 0; waited < ms; waited += 5)
        {
            TestHost.Pump();
            if (condition())
            {
                return true;
            }

            await Task.Delay(5, TestContext.Current.CancellationToken);
        }

        return condition();
    }

    public static async Task PumpAsync(int ms = 30)
    {
        TestHost.Pump();
        await Task.Delay(ms, TestContext.Current.CancellationToken);
        TestHost.Pump();
    }
}
