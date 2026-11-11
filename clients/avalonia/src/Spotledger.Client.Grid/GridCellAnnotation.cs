namespace Spotledger.Client.Grid;

/// <summary>How serious an annotation is. The grid draws each severity as a different
/// shape as well as a different colour, so the marker never relies on colour alone.</summary>
public enum GridAnnotationSeverity
{
    /// <summary>A problem the user must fix (drawn as a triangle).</summary>
    Error,
    /// <summary>A problem the user may accept (drawn as a diamond).</summary>
    Warning,
    /// <summary>A hint (drawn as a circle).</summary>
    Info,
}

/// <summary>A note the data source attaches to a cell or a row: which severity and why.
/// The grid draws a marker, answers the hit-test and shows <see cref="Message"/> as a
/// tooltip; it knows nothing else about what the note means.</summary>
public sealed record GridCellAnnotation
{
    /// <summary>The longest message kept, in UTF-16 code units; longer text is cut at the
    /// source with an ellipsis.</summary>
    public const int MaxMessageLength = 512;

    private readonly string _message = string.Empty;

    /// <summary>Creates an annotation; a message over <see cref="MaxMessageLength"/> is cut.</summary>
    public GridCellAnnotation(GridAnnotationSeverity severity, string message)
    {
        Severity = severity;
        Message = message;
    }

    /// <summary>The severity.</summary>
    public GridAnnotationSeverity Severity { get; init; }

    /// <summary>The tooltip text, never longer than <see cref="MaxMessageLength"/>.</summary>
    public string Message
    {
        get => _message;
        init => _message = Bound(value);
    }

    /// <summary>Cuts text to the bound, ending in an ellipsis and never splitting a
    /// surrogate pair.</summary>
    internal static string Bound(string? text)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        if (text.Length <= MaxMessageLength)
        {
            return text;
        }

        var keep = MaxMessageLength - 1;
        if (char.IsHighSurrogate(text[keep - 1]))
        {
            keep--;
        }

        return string.Concat(text.AsSpan(0, keep), "…");
    }
}
