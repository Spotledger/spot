using Spotledger.Client.Expressions;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>A module in process: reads the request JSON, evaluates it on the desktop evaluator and answers
/// in JSON. It stands in for the WebAssembly module and proves the wire format loses nothing.</summary>
internal sealed class InProcessModule : IWasmCalcModule
{
    public bool IsLoaded { get; set; } = true;

    public int Calls { get; private set; }

    public string Evaluate(string requestJson)
    {
        Calls++;
        return CalcWire.EncodeOutcome(new ManagedCalculationBackend().Evaluate(CalcWire.DecodeRequest(requestJson)));
    }
}

/// <summary>
/// The calculation path gives the desktop results on the expression conformance corpus, with the
/// decimal rounding cases included (the corpus holds them in arithmetic.json and functions.json), on
/// every backend and through the wire format. The desktop result is the corpus expectation itself:
/// the desktop client passes the same corpus in Spotledger.Client.Expressions.Tests.
/// </summary>
public class CalculationConformanceTests
{
    public static TheoryData<string> CorpusFiles()
    {
        var data = new TheoryData<string>();
        foreach (var file in CorpusSupport.Files())
        {
            data.Add(file);
        }

        return data;
    }

    /// <summary>The corpus rule: an error's offset is compared only when the case states one.</summary>
    private static bool MatchesExpectation(CalcOutcome actual, CalcOutcome expected) =>
        expected.IsOk
            ? actual.Same(expected)
            : !actual.IsOk
                && actual.ErrorCode == expected.ErrorCode
                && (expected.Offset is null || actual.Offset == expected.Offset);

    [Theory]
    [MemberData(nameof(CorpusFiles))]
    public void ManagedBackendMatchesCorpus(string file)
    {
        var failures = new List<string>();
        foreach (var c in CorpusSupport.OfFile(file))
        {
            var actual = new ManagedCalculationBackend().Evaluate(CalcWire.DecodeRequest(c.RequestJson));
            var expected = CalcWire.DecodeOutcome(c.ExpectJson);
            if (!MatchesExpectation(actual, expected))
            {
                failures.Add($"{c.Id}: expected {expected}, got {actual}");
            }
        }

        Assert.True(failures.Count == 0, string.Join(Environment.NewLine, failures));
    }

    [Theory]
    [MemberData(nameof(CorpusFiles))]
    public void WasmPathMatchesCorpus(string file)
    {
        var module = new InProcessModule();
        var path = new CalculationPath();
        path.UseModule(module);
        Assert.Equal("wasm", path.ActiveName);
        var failures = new List<string>();
        foreach (var c in CorpusSupport.OfFile(file))
        {
            var actual = path.Evaluate(CalcWire.DecodeRequest(c.RequestJson));
            var expected = CalcWire.DecodeOutcome(c.ExpectJson);
            if (!MatchesExpectation(actual, expected))
            {
                failures.Add($"{c.Id}: expected {expected}, got {actual}");
            }
        }

        Assert.True(failures.Count == 0, string.Join(Environment.NewLine, failures));
        Assert.Equal(CorpusSupport.OfFile(file).Count, module.Calls);
        Assert.Equal(0, path.ModuleFailures);
    }

    [Fact]
    public void RequestSurvivesTheWireWithoutLoss()
    {
        foreach (var c in CorpusSupport.Files().SelectMany(CorpusSupport.OfFile))
        {
            var request = CalcWire.DecodeRequest(c.RequestJson);
            var again = CalcWire.DecodeRequest(CalcWire.EncodeRequest(request));
            Assert.Equal(request.Source, again.Source);
            Assert.Equal(request.Now, again.Now);
            Assert.Equal(request.Context.Count, again.Context.Count);
            foreach (var (path, value) in request.Context)
            {
                Assert.True(value.Same(again.Context[path]), $"{c.Id}: {path}");
            }
        }
    }

    [Fact]
    public void WholeCorpusWasLoaded()
    {
        Assert.Equal(961, CorpusSupport.Count);
        Assert.Equal(12, CorpusSupport.Files().Count());
    }

    [Fact]
    public void DecimalScaleSurvivesTheWire()
    {
        var outcome = CalcWire.DecodeOutcome(CalcWire.EncodeOutcome(CalcOutcome.Ok(new DecimalValue(1250, 2))));
        var value = Assert.IsType<DecimalValue>(outcome.Value);
        Assert.Equal(2, value.Scale);
        Assert.Equal(1250, value.Mantissa);
        Assert.False(outcome.Value!.Same(new DecimalValue(125, 1)));
    }

    [Fact]
    public void MalformedModuleAnswerIsAFormatError()
    {
        Assert.Throws<FormatException>(() => CalcWire.DecodeOutcome("not json"));
        Assert.Throws<FormatException>(() => CalcWire.DecodeOutcome("{}"));
    }
}
