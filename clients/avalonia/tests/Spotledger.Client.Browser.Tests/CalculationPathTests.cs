using Spotledger.Client.Expressions;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>Backend selection and fallback of the calculation path.</summary>
public class CalculationPathTests
{
    private static readonly ExpressionDateTime Now = ExpressionDateTime.Parse("2026-10-05T10:20:30Z")!.Value;

    private static CalcRequest Sum() => new("1.10 + 2.20", Now, new Dictionary<string, ExpressionValue>());

    private sealed class ThrowingModule : IWasmCalcModule
    {
        public bool IsLoaded => true;

        public string Evaluate(string requestJson) => throw new WasmCalcUnavailableException("trap");
    }

    private sealed class GarbageModule : IWasmCalcModule
    {
        public bool IsLoaded => true;

        public string Evaluate(string requestJson) => "<html>";
    }

    [Fact]
    public void StartsOnTheManagedBackend()
    {
        var path = new CalculationPath();
        Assert.Equal("managed", path.ActiveName);
        Assert.Equal("3.30", path.Evaluate(Sum()).Value!.ToString());
    }

    [Fact]
    public void IgnoresAModuleThatIsNotLoaded()
    {
        var path = new CalculationPath();
        path.UseModule(new InProcessModule { IsLoaded = false });
        Assert.Equal("managed", path.ActiveName);
    }

    [Fact]
    public void SwitchesToTheLoadedModuleAndRaisesTheEvent()
    {
        var path = new CalculationPath();
        var raised = 0;
        path.ActiveChanged += (_, _) => raised++;
        var module = new InProcessModule();
        path.UseModule(module);
        Assert.Equal("wasm", path.ActiveName);
        Assert.Equal("3.30", path.Evaluate(Sum()).Value!.ToString());
        Assert.Equal(1, module.Calls);
        Assert.Equal(1, raised);
    }

    [Theory]
    [InlineData(false)]
    [InlineData(true)]
    public void AFailingModuleFallsBackToManagedWithTheSameResult(bool garbage)
    {
        var path = new CalculationPath();
        path.UseModule(garbage ? new GarbageModule() : new ThrowingModule());
        var outcome = path.Evaluate(Sum());
        Assert.Equal("3.30", outcome.Value!.ToString());
        Assert.Equal("managed", path.ActiveName);
        Assert.Equal(1, path.ModuleFailures);
        Assert.Equal("3.30", path.Evaluate(Sum()).Value!.ToString());
        Assert.Equal(1, path.ModuleFailures);
    }

    [Fact]
    public void WasmBackendRefusesAnUnloadedModule()
    {
        var backend = new WasmCalculationBackend(new InProcessModule { IsLoaded = false });
        Assert.Throws<WasmCalcUnavailableException>(() => backend.Evaluate(Sum()));
    }
}
