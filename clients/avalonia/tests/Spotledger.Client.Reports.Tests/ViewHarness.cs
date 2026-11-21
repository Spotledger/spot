using Avalonia.Controls;
using Avalonia.Input;
using Spotledger.Client.Reports;
using Xunit;

namespace Spotledger.Client.Reports.Tests;

public sealed record Shown(ReportView View, ReportViewModel Model, FakeReportGateway Gateway, Window Window);

public static class ViewHarness
{
    public static async Task<Shown> ShowAsync(FakeReportGateway? gateway = null, ReportViewModelOptions? options = null, double width = 1100, double height = 640, bool load = true, IReportSettingsStore? store = null)
    {
        gateway ??= new FakeReportGateway();
        var model = Fx.Model(gateway, store: store, options: options);
        if (load)
        {
            await model.LoadAsync(cancellationToken: TestContext.Current.CancellationToken);
        }

        var view = new ReportView(model);
        var window = TestHost.Show(view, width, height);
        return new Shown(view, model, gateway, window);
    }

    public static async Task<bool> SettleAsync(ReportView view)
        => await Pumping.WaitAsync(() => view.Grid.TryGetRow(0, out _));

    public static void Press(Control target, Key key, KeyModifiers modifiers = KeyModifiers.None)
        => target.RaiseEvent(new KeyEventArgs { Key = key, KeyModifiers = modifiers, RoutedEvent = InputElement.KeyDownEvent });

    public static string? CellText(Spotledger.Client.Grid.SpotGrid grid, int row, int column)
        => grid.TryGetRow(row, out var value) ? value.Cells[column].Text : null;

    public static void TypeInto(ReportView view, string parameter, string text)
    {
        var box = (Spotledger.Client.Controls.SpotTextInput)view.ParameterControls[parameter];
        box.Text = text;
        TestHost.Pump();
    }
}
