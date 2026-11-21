namespace Spotledger.Client.Theme;

/// <summary>
/// Why typed numeric text could not be turned into the canonical edit form. The list is
/// deliberately short and every member is a <em>refusal</em> with a reason: numeric input in a
/// ledger must never guess what a separator meant, because a guess turns <c>1,5</c> into
/// fifteen and the user gets no message.
/// </summary>
public enum NumericInputProblem
{
    /// <summary>The text is a well-formed number and was normalised.</summary>
    None,

    /// <summary>Nothing numeric was typed (null, empty, whitespace, or only a sign).</summary>
    Empty,

    /// <summary>
    /// A grouping separator is somewhere it cannot be: in the fraction part, in an empty group
    /// (<c>1,,234</c>, a trailing <c>1,</c>, a leading <c>,5</c>), or in a group shape that is
    /// neither Western nor South Asian.
    /// </summary>
    MisplacedGrouping,

    /// <summary>
    /// A comma-separator could be read two ways — as a grouping separator or as a decimal
    /// separator — so nothing is assumed: <c>1,5</c>, <c>1,50</c>, <c>12,34</c>, <c>1,5000</c>
    /// and the European <c>1.234,50</c> are all refused rather than read as ten or a hundred
    /// times the intended amount.
    /// </summary>
    AmbiguousSeparator,

    /// <summary>A character that is not a digit, sign, decimal point or grouping separator.</summary>
    UnknownCharacter,
}

/// <summary>
/// The outcome of <see cref="NumeralText.TryNormalizeNumericInput"/>: the canonical Latin edit
/// text when <see cref="Problem"/> is <see cref="NumericInputProblem.None"/>, and otherwise the
/// <b>original text unchanged</b> together with the reason, so the input can keep what the user
/// typed, quote it in its message, and refuse to commit a guessed value.
/// </summary>
/// <param name="Normalized">
/// Sign, Latin digits and at most one decimal point, with well-formed grouping separators
/// removed. The original text when <paramref name="Problem"/> is not <see cref="NumericInputProblem.None"/>.
/// </param>
/// <param name="Problem">Why the text was refused; <see cref="NumericInputProblem.None"/> when it was not.</param>
public readonly record struct NumericInputResult(string Normalized, NumericInputProblem Problem)
{
    /// <summary>True when the text is a well-formed number.</summary>
    public bool IsValid => Problem == NumericInputProblem.None;
}

/// <summary>
/// The message keys a numeric input translates when it refuses text. These are <b>keys</b>, not
/// prose: the control resolves them against the translation catalogue it is given (the client
/// ships Urdu and English catalogues), so this assembly never carries user-facing English and
/// the wording stays with the translators. <see cref="NumeralText.Explain"/> maps a
/// <see cref="NumericInputProblem"/> to one of them.
/// </summary>
public static class NumericInputKeys
{
    /// <summary>Nothing typed yet.</summary>
    public const string Empty = "numeric.empty";

    /// <summary>A grouping separator where it cannot be, or a group shape that is not well formed.</summary>
    public const string MisplacedGrouping = "numeric.misplaced_grouping";

    /// <summary>A separator that could be a grouping or a decimal separator: refused, never guessed.</summary>
    public const string AmbiguousSeparator = "numeric.ambiguous_separator";

    /// <summary>A character that cannot appear in a number.</summary>
    public const string UnknownCharacter = "numeric.unknown_character";

    /// <summary>Every key above, so a catalogue test can prove none is missing.</summary>
    public static IReadOnlyList<string> All { get; } =
    [
        Empty,
        MisplacedGrouping,
        AmbiguousSeparator,
        UnknownCharacter,
    ];
}
