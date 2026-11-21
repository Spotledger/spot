using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>Value adapter attached by the factory. Call Load after draft refresh and CommitTo on save.
/// FieldState has no notification contract; no test-only notification or metadata property is invented.</summary>
public sealed class FieldControlBinding
{
    private readonly Action<FieldValue> _load;
    private readonly Func<(FieldValue? Value, FieldError? Error)> _read;
    private readonly bool _readOnly;
    private readonly Action<FieldValue> _accepted;
    private readonly Action<FieldState> _presentation;
    internal FieldControlBinding(Action<FieldValue> load, Func<(FieldValue?, FieldError?)> read, bool readOnly, Action<FieldValue> accepted, Action<FieldState> presentation)
    { _load = load; _read = read; _readOnly = readOnly; _accepted = accepted; _presentation = presentation; }
    public void Load(FieldValue value) => _load(value);
    /// <summary>Refresh public presentation after a read, permissions change or reveal.</summary>
    public void Load(FieldState state, CurrencyInfo? currency = null)
    {
        ArgumentNullException.ThrowIfNull(state);
        // Scrub a previously readable editor before the host recreates it as masked.
        // This also clears the parser's last-value fast path; a transition must fail closed.
        if (state.IsMasked) { _load(FieldValue.Null); _accepted(FieldValue.Null); CurrencyChanged?.Invoke(null); }
        _presentation(state);
        if (!state.IsMasked) _load(state.Value);
        CurrencyChanged?.Invoke(currency);
    }
    internal Action<CurrencyInfo?>? CurrencyChanged { get; set; }
    public bool CommitTo(FieldState state)
    {
        ArgumentNullException.ThrowIfNull(state);
        if (_readOnly || !state.IsEditable || state.IsMasked) return true;
        var (value, error) = _read();
        if (error is not null) { state.SetErrors([error]); return false; }
        if (value is null) return false;
        state.Value = value;
        _accepted(value);
        state.ClearErrors();
        return true;
    }
}
