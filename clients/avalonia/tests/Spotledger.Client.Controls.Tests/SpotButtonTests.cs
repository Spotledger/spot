using System.Windows.Input;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Controls.Primitives;
using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Threading;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class SpotButtonTests
{
    private sealed class CountingCommand : ICommand
    {
        private readonly Func<bool>? _canExecute;

        public CountingCommand(Func<bool>? canExecute = null) => _canExecute = canExecute;

        public int Executed { get; private set; }

        public event EventHandler? CanExecuteChanged;

        public bool CanExecute(object? parameter) => _canExecute?.Invoke() ?? true;

        public void Execute(object? parameter) => Executed++;

        public void RaiseCanExecuteChanged() => CanExecuteChanged?.Invoke(this, EventArgs.Empty);
    }

    [AvaloniaFact]
    public void IsBusyBlocksCommandExecution()
    {
        var command = new CountingCommand();
        var button = new SpotButton { Content = "Save", Command = command };
        var window = TestHost.Show(button);

        command.RaiseCanExecuteChanged();
        Dispatcher.UIThread.RunJobs();
        button.PerformClickForTests();
        Assert.Equal(1, command.Executed);

        button.IsBusy = true;
        Dispatcher.UIThread.RunJobs();
        button.PerformClickForTests();
        Assert.Equal(1, command.Executed);

        button.IsBusy = false;
        button.PerformClickForTests();
        Assert.Equal(2, command.Executed);
    }

    [AvaloniaFact]
    public void DisabledBlocksCommandExecution()
    {
        var command = new CountingCommand();
        var button = new SpotButton { Content = "Save", Command = command, IsEnabled = false };
        var window = TestHost.Show(button);

        button.PerformClickForTests();
        Assert.Equal(0, command.Executed);
    }

    private static bool HasPseudoClass(Control control, string css)
    {
        // StyledElement.PseudoClasses is internal API; the test asserts through it reflectively.
        var property = typeof(StyledElement).GetProperty("PseudoClasses",
            System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Public);
        var set = property!.GetValue(control)!;
        var contains = set.GetType().GetMethod("Contains", [typeof(string)])!;
        return (bool)contains.Invoke(set, [css])!;
    }

    [AvaloniaTheory]
    [InlineData(SpotButtonVariant.Primary, ":primary")]
    [InlineData(SpotButtonVariant.Secondary, ":secondary")]
    [InlineData(SpotButtonVariant.Danger, ":danger")]
    [InlineData(SpotButtonVariant.Subtle, ":subtle")]
    public void VariantAppliesItsPseudoClass(SpotButtonVariant variant, string css)
    {
        var button = new SpotButton { Variant = variant };
        TestHost.Show(button);
        Dispatcher.UIThread.RunJobs();
        Assert.True(HasPseudoClass(button, css));
    }

    [AvaloniaFact]
    public void BusyRaisesProgressIndicatorVisibility()
    {
        var button = new SpotButton { Content = "Save" };
        TestHost.Show(button);
        var progress = button.GetTemplateControl<ProgressBar>("PART_Progress");
        Assert.NotNull(progress);
        Assert.False(progress!.IsVisible);
        button.IsBusy = true;
        Dispatcher.UIThread.RunJobs();
        Assert.True(progress.IsVisible);
    }
}

internal static class ControlTestExtensions
{
    /// <summary>Invokes the button click pipeline the same way a pointer release would.</summary>
    public static void PerformClickForTests(this SpotButton button)
    {
        var method = typeof(Button)
            .GetMethod("PerformClick", System.Reflection.BindingFlags.Instance | System.Reflection.BindingFlags.NonPublic);
        Assert.NotNull(method);
        method!.Invoke(button, null);
        Dispatcher.UIThread.RunJobs();
    }

    /// <summary>Finds a named templated child (all template parts are TemplatedControl).</summary>
    public static T? GetTemplateControl<T>(this Control control, string name) where T : TemplatedControl
    {
        foreach (var child in Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(control))
        {
            if (child is T typed && string.Equals(typed.Name, name, StringComparison.Ordinal))
            {
                return typed;
            }
        }

        return null;
    }
}
