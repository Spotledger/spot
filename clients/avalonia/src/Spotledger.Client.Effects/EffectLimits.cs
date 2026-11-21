namespace Spotledger.Client.Effects;

/// <summary>
/// The caps a batch is bounded by, mirroring the server's
/// <c>crates/framework/spot-form-effects/src/validate.rs</c> defaults: the client refuses the
/// same oversized batches the server would never have emitted, so a hostile or buggy server
/// cannot make a form paint a megabyte of labels.
/// </summary>
/// <param name="MaxEffects">The most effects one batch may carry.</param>
/// <param name="MaxOptions">The most candidate values one <c>set_options</c> may carry.</param>
/// <param name="MaxTextChars">The longest message text.</param>
/// <param name="MaxLabelChars">The longest label, option label or action label.</param>
/// <param name="MaxFilterChars">The longest link filter expression.</param>
/// <param name="MaxBatchDepth">The deepest nesting a <c>batch</c> effect may reach.</param>
public sealed record EffectLimits(
    int MaxEffects = 200,
    int MaxOptions = 500,
    int MaxTextChars = 4000,
    int MaxLabelChars = 200,
    int MaxFilterChars = 2000,
    int MaxBatchDepth = EffectsProtocol.MaxBatchDepth)
{
    /// <summary>The server's defaults.</summary>
    public static EffectLimits Default { get; } = new();
}
