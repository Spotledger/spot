using System.Globalization;
using Spotledger.Client.Expressions;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Extensions;

/// <summary>
/// The values and facts one rendering sees: the document, its selection, the user, the
/// locale, the permissions the server already filtered for, and the instant
/// <c>now()</c> returns. The context is the only outside world an expression can read, and
/// it never reads a clock: time is an argument, so a test is deterministic.
///
/// The context is also the change source of a <see cref="SlotView" />: it re-raises
/// <see cref="Changed" /> when the draft's fields change, so visibility and enabled state
/// are re-evaluated when the user edits the form.
/// </summary>
public sealed class ExtensionContext : IExpressionContext, IDisposable
{
    private readonly Func<string, FieldValue?>? _reader;
    private DocumentDraft? _draft;
    private bool _disposed;

    /// <summary>Creates a context. Every fact is an argument; nothing is read from the environment.</summary>
    /// <param name="now">The instant <c>now()</c> returns.</param>
    /// <param name="locale">The user's locale tag, for formatting and direction.</param>
    /// <param name="user">The signed-in user id, when the shell knows it.</param>
    /// <param name="permissions">The permissions the server resolved for this user and document.</param>
    /// <param name="selection">The selected row ids of a list, which a host sends as <c>ids</c>.</param>
    /// <param name="document">Reads one field value by storage name; null when there is no document.</param>
    /// <param name="currencySymbol">The display symbol for <c>currency</c> presentations, when the shell knows it.</param>
    public ExtensionContext(
        ExpressionDateTime now,
        string locale = "en",
        string? user = null,
        IEnumerable<string>? permissions = null,
        IReadOnlyList<string>? selection = null,
        Func<string, FieldValue?>? document = null,
        string? currencySymbol = null)
    {
        Now = now;
        Locale = locale;
        User = user;
        Permissions = new HashSet<string>(permissions ?? [], StringComparer.Ordinal);
        Selection = selection ?? [];
        _reader = document;
        CurrencySymbol = currencySymbol;
    }

    /// <summary>An empty context: no document, no selection, no permissions.</summary>
    public static ExtensionContext Empty(ExpressionDateTime now, string locale = "en") => new(now, locale);

    /// <summary>The instant <c>now()</c> returns.</summary>
    public ExpressionDateTime Now { get; }

    /// <summary>The user's locale tag.</summary>
    public string Locale { get; }

    /// <summary>The signed-in user id, when known. Display only: the server re-checks every call.</summary>
    public string? User { get; }

    /// <summary>The permissions the server resolved; an item whose <c>permission</c> is absent is hidden.</summary>
    public IReadOnlySet<string> Permissions { get; }

    /// <summary>The selected row ids of the list the host is showing, sent as <c>ids</c>.</summary>
    public IReadOnlyList<string> Selection { get; }

    /// <summary>The display symbol of a <c>currency</c> presentation, when the shell knows it.</summary>
    public string? CurrencySymbol { get; }

    /// <summary>The document being edited, when the context was built from one.</summary>
    public DocumentDraft? Draft => _draft;

    /// <summary>The document's id, for the <c>If-Match</c> version of a mutating call.</summary>
    public string? DocumentId => _draft?.DocumentId;

    /// <summary>The document's version, for the <c>If-Match</c> version of a mutating call.</summary>
    public int? DocumentVersion => _draft is null ? null : _draft.Version;

    /// <summary>Raised when the draft's fields or structure change.</summary>
    public event EventHandler? Changed;

    /// <summary>
    /// Builds a context over a draft and subscribes to its changes, so a slot view over
    /// this context re-evaluates when the user edits a field.
    /// </summary>
    public static ExtensionContext FromDocument(
        DocumentDraft draft,
        ExpressionDateTime now,
        string locale = "en",
        string? user = null,
        IEnumerable<string>? permissions = null,
        IReadOnlyList<string>? selection = null,
        string? currencySymbol = null)
    {
        ArgumentNullException.ThrowIfNull(draft);
        ExtensionContext context = new(now, locale, user, permissions, selection, draft.Get, currencySymbol)
        {
            _draft = draft
        };
        draft.FieldChanged += context.OnDraftFieldChanged;
        draft.StructureChanged += context.OnDraftStructureChanged;
        return context;
    }

    /// <summary>Builds a context over a flat field map (tests and hosts that keep no draft).</summary>
    public static ExtensionContext FromValues(
        IReadOnlyDictionary<string, FieldValue> values,
        ExpressionDateTime now,
        string locale = "en",
        string? user = null,
        IEnumerable<string>? permissions = null,
        IReadOnlyList<string>? selection = null)
    {
        ArgumentNullException.ThrowIfNull(values);
        return new ExtensionContext(now, locale, user, permissions, selection,
            name => values.TryGetValue(name, out FieldValue? value) ? value : null);
    }

    /// <summary>
    /// The arguments a host always adds to a list-scoped call: the selected ids as
    /// <c>ids</c>. Empty when nothing is selected.
    /// </summary>
    public IReadOnlyList<KeyValuePair<string, ExpressionValue>> SelectionArguments()
    {
        if (Selection.Count == 0)
        {
            return [];
        }
        ExpressionValue[] values = new ExpressionValue[Selection.Count];
        for (int index = 0; index < Selection.Count; index++)
        {
            values[index] = new TextValue(Selection[index]);
        }
        return [new KeyValuePair<string, ExpressionValue>("ids", new ListValue(values))];
    }

    /// <summary>The value at <c>root.names...</c>; only <c>doc.&lt;field&gt;</c> resolves, and only when a document is present.</summary>
    public ExpressionValue? Field(ExpressionRoot root, IReadOnlyList<string> names)
    {
        ArgumentNullException.ThrowIfNull(names);
        if (root != ExpressionRoot.Doc || _reader is null || names.Count != 1)
        {
            return null;
        }
        FieldValue? value = _reader(names[0]);
        return value is null ? null : DocValues.ToExpression(value);
    }

    /// <summary>The permissions a call needs; a permission the context lacks hides the item.</summary>
    public bool HasPermission(string? permission) =>
        string.IsNullOrEmpty(permission) || Permissions.Contains(permission);

    /// <summary>Re-raises <see cref="Changed" />; the shell may call it after a refresh.</summary>
    public void NotifyChanged() => Changed?.Invoke(this, EventArgs.Empty);

    /// <summary>Unsubscribes from the draft.</summary>
    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }
        _disposed = true;
        if (_draft is not null)
        {
            _draft.FieldChanged -= OnDraftFieldChanged;
            _draft.StructureChanged -= OnDraftStructureChanged;
        }
        _draft = null;
    }

    private void OnDraftFieldChanged(object? sender, string field) => Changed?.Invoke(this, EventArgs.Empty);

    private void OnDraftStructureChanged(object? sender, EventArgs e) => Changed?.Invoke(this, EventArgs.Empty);
}

/// <summary>Converts the metadata runtime core's typed values to the expression language's
/// values without losing precision: a decimal stays mantissa and scale, never a double.</summary>
public static class DocValues
{
    /// <summary>Converts one field value to an expression value.</summary>
    public static ExpressionValue ToExpression(FieldValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return value switch
        {
            NullFieldValue => ExpressionValue.Null,
            BoolFieldValue boolean => new BoolValue(boolean.Value),
            IntFieldValue integer => new IntValue(integer.Value),
            DecimalFieldValue exact => DecimalFromCanonical(exact.CanonicalText),
            TextFieldValue text => new TextValue(text.Value),
            SelectFieldValue select => new TextValue(select.Value),
            MultiSelectFieldValue multiple => new ListValue([.. multiple.Values.Select(static item => (ExpressionValue)new TextValue(item))]),
            DateFieldValue date => new DateValue(ExpressionDate.FromYearMonthDay(date.Value.Year, date.Value.Month, date.Value.Day) ?? default),
            TimeFieldValue time => new TextValue(time.Value.ToString("HH:mm:ss", CultureInfo.InvariantCulture)),
            DateTimeFieldValue instant => new DateTimeValue(ExpressionDateTime.FromMicros(instant.Value.ToUnixTimeMilliseconds() * 1000L) ?? default),
            LinkFieldValue link => new TextValue(link.DisplayText ?? link.Id ?? string.Empty),
            AttachFieldValue attach => new TextValue(attach.FileId ?? string.Empty),
            SecretFieldValue secret => new TextValue(secret.IsSet ? "set" : string.Empty),
            BytesFieldValue bytes => new TextValue(bytes.Base64),
            JsonFieldValue json => new TextValue(json.JsonText),
            TableFieldValue table => new ListValue([.. table.Rows.Select(static row => (ExpressionValue)new TextValue(row.Id ?? string.Empty))]),
            _ => ExpressionValue.Null
        };
    }

    /// <summary>
    /// Builds an exact decimal expression value from the canonical text (<c>-12.3400</c>).
    /// Digits beyond <see cref="Int128" /> or a malformed text fall back to the text value,
    /// so a hostile document cannot make the adapter throw.
    /// </summary>
    public static ExpressionValue DecimalFromCanonical(string canonical)
    {
        ArgumentNullException.ThrowIfNull(canonical);
        string text = canonical.Trim();
        bool negative = text.StartsWith('-');
        if (negative || text.StartsWith('+'))
        {
            text = text[1..];
        }
        int point = text.IndexOf('.', StringComparison.Ordinal);
        string digits = point < 0 ? text : text.Remove(point, 1);
        int scale = point < 0 ? 0 : text.Length - point - 1;
        if (digits.Length == 0 || digits.Length > 38 || !digits.All(char.IsAsciiDigit) ||
            !Int128.TryParse(digits, NumberStyles.None, CultureInfo.InvariantCulture, out Int128 mantissa))
        {
            return new TextValue(canonical);
        }
        return new DecimalValue(negative ? -mantissa : mantissa, scale);
    }
}

/// <summary>The display text of a metadata value for read-only presentations (field lists,
/// table cells, composite number/date controls). Exact: a decimal is its canonical text.</summary>
public static class DisplayText
{
    /// <summary>The text one value shows as.</summary>
    public static string For(FieldValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return value switch
        {
            NullFieldValue => string.Empty,
            BoolFieldValue boolean => boolean.Value ? "true" : "false",
            IntFieldValue integer => integer.Value.ToString(CultureInfo.InvariantCulture),
            DecimalFieldValue exact => exact.CanonicalText,
            TextFieldValue text => text.Value,
            SelectFieldValue select => select.Value,
            MultiSelectFieldValue multiple => string.Join(", ", multiple.Values),
            DateFieldValue date => date.Value.ToString("yyyy-MM-dd", CultureInfo.InvariantCulture),
            TimeFieldValue time => time.Value.ToString("HH:mm:ss", CultureInfo.InvariantCulture),
            DateTimeFieldValue instant => instant.Value.ToString("yyyy-MM-dd HH:mm:ss", CultureInfo.InvariantCulture),
            LinkFieldValue link => link.DisplayText ?? link.Id ?? string.Empty,
            AttachFieldValue attach => attach.FileId ?? string.Empty,
            SecretFieldValue secret => secret.IsSet ? "\u2022\u2022\u2022\u2022" : string.Empty,
            BytesFieldValue bytes => bytes.Base64,
            JsonFieldValue json => json.JsonText,
            TableFieldValue table => table.Rows.Count.ToString(CultureInfo.InvariantCulture) + (table.Rows.Count == 1 ? " row" : " rows"),
            _ => string.Empty
        };
    }

    /// <summary>The text of an expression value for a read-only presentation.</summary>
    public static string For(ExpressionValue value)
    {
        ArgumentNullException.ThrowIfNull(value);
        return value switch
        {
            NullValue => string.Empty,
            BoolValue boolean => boolean.Value ? "true" : "false",
            IntValue integer => integer.Value.ToString(CultureInfo.InvariantCulture),
            DecimalValue exact => exact.ToString(),
            TextValue text => text.Value,
            DateValue date => date.Value.ToString(),
            DateTimeValue instant => instant.Value.ToString(),
            ListValue list => string.Join(", ", list.Values.Select(For)),
            _ => string.Empty
        };
    }
}
