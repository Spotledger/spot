namespace Spotledger.Client.Effects;

/// <summary>
/// The presentation state an effect batch sets on a form: which fields the batch made
/// read-only, hidden or required, the candidate values and labels it replaced, the link
/// filters it set, the keyboard focus it moved, and the actions it added. Empty when no
/// effect touched the form.
/// </summary>
/// <remarks>
/// These are overrides, not the whole answer: a renderer asks this state together with the
/// metadata (a field the DocType declares read-only stays read-only) through
/// <see cref="FormState.IsReadOnly"/>, <see cref="FormState.IsHidden"/> and
/// <see cref="FormState.IsRequired"/>. Paths are wire paths, so a row column carries
/// <c>items[2].note</c>.
/// </remarks>
public sealed class PresentationState
{
    private readonly HashSet<string> _readOnly = new(StringComparer.Ordinal);
    private readonly HashSet<string> _hidden = new(StringComparer.Ordinal);
    private readonly HashSet<string> _required = new(StringComparer.Ordinal);
    private readonly Dictionary<string, IReadOnlyList<OptionItem>> _options = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _labels = new(StringComparer.Ordinal);
    private readonly Dictionary<string, string> _filters = new(StringComparer.Ordinal);

    /// <summary>The paths the batch made read-only, in no particular order.</summary>
    public IReadOnlyCollection<string> ReadOnlyPaths => _readOnly;

    /// <summary>The paths the batch hid.</summary>
    public IReadOnlyCollection<string> HiddenPaths => _hidden;

    /// <summary>The paths the batch marked required.</summary>
    public IReadOnlyCollection<string> RequiredPaths => _required;

    /// <summary>The candidate values the batch replaced, by path.</summary>
    public IReadOnlyDictionary<string, IReadOnlyList<OptionItem>> Options => _options;

    /// <summary>The labels the batch replaced, by path.</summary>
    public IReadOnlyDictionary<string, string> Labels => _labels;

    /// <summary>The link filters the batch set, by path.</summary>
    public IReadOnlyDictionary<string, string> Filters => _filters;

    /// <summary>The path the batch moved the keyboard focus to, when it moved it.</summary>
    public string? Focus { get; internal set; }

    /// <summary>Whether the batch made <paramref name="path"/> read-only.</summary>
    public bool IsReadOnly(string path) => _readOnly.Contains(path);

    /// <summary>Whether the batch hid <paramref name="path"/>.</summary>
    public bool IsHidden(string path) => _hidden.Contains(path);

    /// <summary>Whether the batch marked <paramref name="path"/> required.</summary>
    public bool IsRequired(string path) => _required.Contains(path);

    /// <summary>The candidate values the batch set for <paramref name="path"/>, when it did.</summary>
    public IReadOnlyList<OptionItem>? OptionsFor(string path) =>
        _options.TryGetValue(path, out IReadOnlyList<OptionItem>? options) ? options : null;

    /// <summary>The label the batch set for <paramref name="path"/>, when it did.</summary>
    public string? LabelFor(string path) => _labels.TryGetValue(path, out string? label) ? label : null;

    /// <summary>The link filter the batch set for <paramref name="path"/>, when it did.</summary>
    public string? FilterFor(string path) => _filters.TryGetValue(path, out string? filter) ? filter : null;

    /// <summary>Sets of clears a read-only override.</summary>
    public void SetReadOnly(string path, bool readOnly) => SetFlag(_readOnly, path, readOnly);

    /// <summary>Sets or clears a hidden override.</summary>
    public void SetHidden(string path, bool hidden) => SetFlag(_hidden, path, hidden);

    /// <summary>Sets or clears a required override.</summary>
    public void SetRequired(string path, bool required) => SetFlag(_required, path, required);

    /// <summary>Replaces the candidate values of a select or link field.</summary>
    public void SetOptions(string path, IReadOnlyList<OptionItem> options) => _options[path] = options;

    /// <summary>Replaces a field's label.</summary>
    public void SetLabel(string path, string label) => _labels[path] = label;

    /// <summary>Sets a link field's filter expression.</summary>
    public void SetFilter(string path, string filter) => _filters[path] = filter;

    /// <summary>Forgets the focus request a previous round raised.</summary>
    internal void ClearFocus() => Focus = null;

    private static void SetFlag(HashSet<string> set, string path, bool on)
    {
        if (on)
        {
            set.Add(path);
        }
        else
        {
            set.Remove(path);
        }
    }
}
