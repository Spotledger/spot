using Spotledger.Client.Expressions;

namespace Spotledger.Client.Browser;

/// <summary>One way to evaluate a calculation preview. Backends give identical outcomes.</summary>
public interface ICalculationBackend
{
    /// <summary>A short stable name for the metrics (<c>managed</c>, <c>wasm</c>).</summary>
    string Name { get; }

    /// <summary>Evaluates the request. Never throws for a bad expression: the outcome carries the error.</summary>
    CalcOutcome Evaluate(CalcRequest request);
}

/// <summary>
/// The reference backend: the shared expression language of the desktop client
/// (<c>Spotledger.Client.Expressions</c>, the .NET twin of the Rust crate <c>spot-expr</c>), run in the
/// browser's .NET runtime. It is what desktop runs, so its results are the desktop results by
/// construction; it is also the fallback while the WebAssembly module is not (yet) loaded.
/// </summary>
public sealed class ManagedCalculationBackend : ICalculationBackend
{
    /// <inheritdoc />
    public string Name => "managed";

    /// <inheritdoc />
    public CalcOutcome Evaluate(CalcRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        try
        {
            return CalcOutcome.Ok(Expression.Evaluate(request.Source, request.ToContext()));
        }
        catch (ExpressionParseException exception)
        {
            return CalcOutcome.Error(exception.Code, exception.Offset);
        }
        catch (ExpressionEvaluationException exception)
        {
            return CalcOutcome.Error(exception.Code);
        }
    }
}
