using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Spotledger.Component.Contract;
using Spotledger.Component.Host;
using Xunit;

namespace Spotledger.Component.Host.Tests;

/// <summary>
/// Failure isolation: a component that throws, or that overruns its budget, becomes a placeholder and
/// an error entry — never an exception that unwinds the form.
/// </summary>
public sealed class SlotTests
{
    [AvaloniaFact]
    public void AThrowingConstructorShowsAPlaceholderAndAnErrorEntry()
    {
        var log = new RecordingLog();
        var slot = new ComponentSlot(Guard(), log);

        var view = slot.Open(TestDescriptors.Throwing(), TestDescriptors.Context());

        Assert.True(view.IsPlaceholder);
        Assert.Null(view.Component);
        var error = Assert.Single(slot.Errors);
        Assert.Equal(ComponentRefusalCodes.ComponentThrew, error.Refusal.Code);
        Assert.Contains("test.throwing", error.Refusal.Message, StringComparison.Ordinal);

        var border = Assert.IsType<Border>(view.View);
        var text = Assert.IsType<TextBlock>(border.Child);
        Assert.Contains(ComponentRefusalCodes.ComponentThrew, text.Text!, StringComparison.Ordinal);
        Assert.Equal(ComponentRefusalCodes.ComponentThrew, Assert.Single(log.Failures).Code);
    }

    [AvaloniaFact]
    public void AComponentThatIsSlowToConstructIsNotPutInTheSlot()
    {
        var log = new RecordingLog();
        var clock = new FakeClock { StepTicks = TimeSpan.TicksPerMillisecond * 5 };
        var slot = new ComponentSlot(
            new ComponentResourceGuard(clock, TimeSpan.FromMilliseconds(1), TimeSpan.FromMilliseconds(1), log),
            log);

        var view = slot.Open(TestDescriptors.Stub("test.slow"), TestDescriptors.Context());

        Assert.True(view.IsPlaceholder);
        var error = Assert.Single(slot.Errors);
        Assert.Equal(ComponentRefusalCodes.ConstructorBudgetExceeded, error.Refusal.Code);
        Assert.Contains("The component was not put in the slot", error.Refusal.Message, StringComparison.Ordinal);
    }

    [AvaloniaFact]
    public void AGoodComponentOpensAndShowsItsOwnControl()
    {
        var slot = new ComponentSlot(Guard());

        var view = slot.Open(TestDescriptors.Stub("test.stub"), TestDescriptors.Context());

        Assert.False(view.IsPlaceholder);
        Assert.NotNull(view.Component);
        Assert.Same(view.Component!.View, view.View);
        Assert.Empty(slot.Errors);
    }

    [AvaloniaFact]
    public void AThrowingUpdateIsAnErrorEntryAndTheSlotKeepsTheComponent()
    {
        var log = new RecordingLog();
        var slot = new ComponentSlot(Guard(), log);
        var view = slot.Open(TestDescriptors.Stub("test.stub", throwOnUpdate: true), TestDescriptors.Context());

        var outcome = slot.Update(view, ComponentValue.FromInteger(5));

        Assert.NotNull(outcome);
        Assert.False(outcome.WithinBudget);
        Assert.Equal(ComponentRefusalCodes.ComponentThrew, outcome.Refusal!.Code);
        Assert.False(view.IsPlaceholder);
        var error = Assert.Single(slot.Errors);
        Assert.Equal(ComponentRefusalCodes.ComponentThrew, error.Refusal.Code);
        Assert.Single(log.Failures);
    }

    [AvaloniaFact]
    public void AnUpdateThatOverrunsItsBudgetStandsAndIsReported()
    {
        var log = new RecordingLog();
        var clock = new FakeClock { StepTicks = TimeSpan.TicksPerMillisecond * 5 };
        var slot = new ComponentSlot(
            new ComponentResourceGuard(clock, TimeSpan.FromMilliseconds(1_000), TimeSpan.FromMilliseconds(1), log),
            log);
        var view = slot.Open(TestDescriptors.Stub("test.stub"), TestDescriptors.Context());

        var outcome = slot.Update(view, ComponentValue.FromInteger(5));

        Assert.NotNull(outcome);
        Assert.False(outcome.WithinBudget);
        Assert.Equal(ComponentRefusalCodes.UpdateBudgetExceeded, outcome.Refusal!.Code);
        Assert.True(outcome.Value);
        var error = Assert.Single(slot.Errors);
        Assert.Equal(ComponentRefusalCodes.UpdateBudgetExceeded, error.Refusal.Code);
        Assert.Equal(5, ((StubComponent)view.Component!).Value.TryGetInteger(out var stars) ? stars : -1);
    }

    [AvaloniaFact]
    public void AnUpdateOnAPlaceholderDoesNothing()
    {
        var slot = new ComponentSlot(Guard());
        var view = slot.Open(TestDescriptors.Throwing(), TestDescriptors.Context());

        Assert.Null(slot.Update(view, ComponentValue.FromInteger(5)));
    }

    [AvaloniaFact]
    public void APlaceholderCanBeBuiltOnItsOwn()
    {
        var control = ComponentSlot.BuildPlaceholder("acme.rating", ComponentRefusalCodes.ComponentThrew);

        var border = Assert.IsType<Border>(control);
        var text = Assert.IsType<TextBlock>(border.Child);
        Assert.Contains("acme.rating", text.Text!, StringComparison.Ordinal);
        Assert.Contains("unavailable", text.Text!, StringComparison.Ordinal);
    }

    private static ComponentResourceGuard Guard(RecordingLog? log = null) =>
        new(new FakeClock(), TimeSpan.FromSeconds(1), TimeSpan.FromSeconds(1), log);
}
