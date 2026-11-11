namespace Spotledger.Client.Browser;

/// <summary>
/// A calculation WebAssembly module as the .NET side sees it (its JavaScript half is
/// <c>wwwroot/calc.mjs</c>): text in, text out, the JSON of <see cref="CalcWire"/>. The module is not
/// part of the first-paint path: it loads after the shell is ready, and until it has loaded
/// <see cref="CalculationPath"/> answers with the managed backend.
/// </summary>
public interface IWasmCalcModule
{
    /// <summary>True once the module has been fetched and instantiated.</summary>
    bool IsLoaded { get; }

    /// <summary>Evaluates one request (<see cref="CalcWire.EncodeRequest"/>) and returns the outcome JSON.
    /// Throws <see cref="WasmCalcUnavailableException"/> when the module cannot answer.</summary>
    string Evaluate(string requestJson);
}

/// <summary>The module could not answer: it is not loaded, trapped, or ran out of memory.</summary>
public sealed class WasmCalcUnavailableException : Exception
{
    /// <summary>Creates the exception.</summary>
    public WasmCalcUnavailableException(string message)
        : base(message)
    {
    }

    /// <summary>Creates the exception with the cause.</summary>
    public WasmCalcUnavailableException(string message, Exception innerException)
        : base(message, innerException)
    {
    }
}

/// <summary>The backend that sends every request through a <see cref="IWasmCalcModule"/> as JSON.</summary>
public sealed class WasmCalculationBackend : ICalculationBackend
{
    private readonly IWasmCalcModule _module;

    /// <summary>Creates the backend over a module.</summary>
    public WasmCalculationBackend(IWasmCalcModule module)
    {
        ArgumentNullException.ThrowIfNull(module);
        _module = module;
    }

    /// <inheritdoc />
    public string Name => "wasm";

    /// <inheritdoc />
    public CalcOutcome Evaluate(CalcRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (!_module.IsLoaded)
        {
            throw new WasmCalcUnavailableException("the calculation module is not loaded");
        }

        return CalcWire.DecodeOutcome(_module.Evaluate(CalcWire.EncodeRequest(request)));
    }
}

/// <summary>
/// The calculation path of the browser head (SPK-07: totals, tax and pricing previews run on the
/// client). It answers every request with the first backend that can: the WebAssembly module when it
/// is loaded, the managed backend otherwise, and the managed backend again if the module fails during a
/// request (the failure is counted and the module is dropped, so one bad module never breaks an
/// entry form). It never changes a result: both backends give the outcome the conformance corpus
/// demands, which the head's tests check on every case.
/// </summary>
public sealed class CalculationPath
{
    private readonly ManagedCalculationBackend _managed = new();
    private ICalculationBackend _primary;

    /// <summary>Creates the path on the managed backend.</summary>
    public CalculationPath()
    {
        _primary = _managed;
    }

    /// <summary>The name of the backend that answers the next request.</summary>
    public string ActiveName => _primary.Name;

    /// <summary>How many requests the WebAssembly module failed (answered by the managed backend instead).</summary>
    public int ModuleFailures { get; private set; }

    /// <summary>Raised when <see cref="ActiveName"/> changes.</summary>
    public event EventHandler? ActiveChanged;

    /// <summary>Switches to the module once it has loaded. A module that is not loaded is ignored.</summary>
    public void UseModule(IWasmCalcModule module)
    {
        ArgumentNullException.ThrowIfNull(module);
        if (!module.IsLoaded)
        {
            return;
        }

        _primary = new WasmCalculationBackend(module);
        ActiveChanged?.Invoke(this, EventArgs.Empty);
    }

    /// <summary>Evaluates a request on the active backend, falling back to the managed one on a module failure.</summary>
    public CalcOutcome Evaluate(CalcRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        try
        {
            return _primary.Evaluate(request);
        }
        catch (Exception exception) when (exception is WasmCalcUnavailableException or FormatException)
        {
            ModuleFailures++;
            _primary = _managed;
            ActiveChanged?.Invoke(this, EventArgs.Empty);
            return _managed.Evaluate(request);
        }
    }
}
