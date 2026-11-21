namespace Spotledger.Client.Metadata;

/// <summary>
/// The state of one field in a <see cref="DocumentDraft"/>: the current value, the
/// value the document was loaded with, whether they differ, and the errors currently
/// attached (from validation or from the server's response).
/// </summary>
public sealed class FieldState
{
    /// <summary>Creates the state (the draft's constructor and revert paths build these).</summary>
    public FieldState(FieldValue original, FieldModel? model = null)
    {
        Original = original;
        _value = original;
        IsVisible = model?.Hints.Hidden != true;
        IsMandatory = model?.Required == true;
        IsReadOnly = model?.ReadOnly == true;
    }

    /// <summary>Whether the field is visible in the effective metadata.</summary>
    public bool IsVisible { get; internal set; }

    /// <summary>Whether a value is required by the effective metadata.</summary>
    public bool IsMandatory { get; internal set; }

    /// <summary>Whether the effective metadata prohibits editing.</summary>
    public bool IsReadOnly { get; internal set; }

    private FieldValue _value;

    /// <summary>The value the document was loaded with (or the default a fresh draft
    /// started from, which becomes the original the moment the draft is saved).</summary>
    public FieldValue Original { get; private set; }

    /// <summary>The current value.</summary>
    public FieldValue Value
    {
        get => _value;
        set
        {
            if (IsMasked)
            {
                throw new InvalidOperationException("A masked field cannot hold an editable value.");
            }
            _value = value ?? FieldValue.Null;
        }
    }

    /// <summary>True when the permission snapshot permits only masked presentation.</summary>
    public bool IsMasked { get; private set; }
    /// <summary>Server-provided display text, never a normal value or a write operand.</summary>
    public string? MaskedText { get; private set; }
    /// <summary>Whether the current snapshot offers the audited reveal command.</summary>
    public bool CanReveal { get; private set; }
    /// <summary>The separately decoded reveal result; read-only and never saved.</summary>
    public FieldValue? RevealedValue { get; private set; }
    /// <summary>The caller's effective attachment constraint, or none.</summary>
    public AttachLimit? AttachLimit { get; private set; }
    /// <summary>Derived from rights.create of the link target in the same snapshot.</summary>
    public bool CanCreateTarget { get; private set; }
    /// <summary>True when metadata and the permission summary allow editing.</summary>
    public bool IsEditable { get; private set; } = true;

    internal void ApplyPermissions(bool masked, bool revealable, AttachLimit? limit, bool createTarget, bool editable)
    {
        if (masked != IsMasked)
        {
            MaskedText = null;
            _value = Original = FieldValue.Null;
        }
        IsMasked = masked;
        CanReveal = masked && revealable;
        AttachLimit = limit;
        CanCreateTarget = createTarget;
        IsEditable = editable && !masked;
        RevealedValue = null;
    }

    internal void LoadMasked(string? text)
    {
        MaskedText = text;
        _value = Original = FieldValue.Null;
        RevealedValue = null;
        ClearErrors();
    }

    internal void SetRevealed(FieldValue value)
    {
        if (!IsMasked || !CanReveal)
        {
            throw new InvalidOperationException("This field does not offer reveal.");
        }
        RevealedValue = value;
    }

    internal void ClearRevealed() => RevealedValue = null;

    internal void ReplaceLoadedValue(FieldValue value)
    {
        _value = Original = IsMasked ? FieldValue.Null : value;
        RevealedValue = null;
        ClearErrors();
    }

    internal FieldState SavedCopy() => new(Original)
    {
        IsMasked = IsMasked, MaskedText = MaskedText, CanReveal = CanReveal,
        AttachLimit = AttachLimit, CanCreateTarget = CanCreateTarget, IsEditable = IsEditable,
    };

    /// <summary>True when the current value differs from the original (exact decimal
    /// comparison; a changed scale of the same number is not dirty).</summary>
    public bool IsDirty => !ValuesEqual(Original, _value);

    /// <summary>The errors currently attached to the field (local validation and the
    /// server's field errors), in arrival order.</summary>
    public IReadOnlyList<FieldError> Errors => _errors;

    private readonly List<FieldError> _errors = new();

    /// <summary>Replaces the attached errors.</summary>
    public void SetErrors(IEnumerable<FieldError> errors)
    {
        _errors.Clear();
        _errors.AddRange(errors);
    }

    /// <summary>Clears the attached errors.</summary>
    public void ClearErrors() => _errors.Clear();

    /// <summary>Makes the current value the original (after a save, before the next edit).</summary>
    public void MarkSaved()
    {
        Original = _value;
        ClearErrors();
    }

    /// <summary>The value comparison the dirty flag and the patch builder share.</summary>
    internal static bool ValuesEqual(FieldValue a, FieldValue b)
    {
        if (ReferenceEquals(a, b))
        {
            return true;
        }

        if (a is null || b is null)
        {
            return a is null && b is null;
        }

        return (a, b) switch
        {
            (NullFieldValue, NullFieldValue) => true,
            (BoolFieldValue x, BoolFieldValue y) => x.Value == y.Value,
            (IntFieldValue x, IntFieldValue y) => x.Value == y.Value,
            (DecimalFieldValue x, DecimalFieldValue y) => x.ExactValue.Equals(y.ExactValue),
            (TextFieldValue x, TextFieldValue y) => string.Equals(x.Value, y.Value, StringComparison.Ordinal),
            (SelectFieldValue x, SelectFieldValue y) => string.Equals(x.Value, y.Value, StringComparison.Ordinal),
            (MultiSelectFieldValue x, MultiSelectFieldValue y) => x.Values.SequenceEqual(y.Values, StringComparer.Ordinal),
            (DateFieldValue x, DateFieldValue y) => x.Value.Equals(y.Value),
            (TimeFieldValue x, TimeFieldValue y) => x.Value.Equals(y.Value),
            (DateTimeFieldValue x, DateTimeFieldValue y) => x.Value.Equals(y.Value),
            (LinkFieldValue x, LinkFieldValue y) =>
                string.Equals(x.Id, y.Id, StringComparison.Ordinal)
                && string.Equals(x.DisplayText, y.DisplayText, StringComparison.Ordinal),
            (AttachFieldValue x, AttachFieldValue y) => string.Equals(x.FileId, y.FileId, StringComparison.Ordinal),
            (SecretFieldValue x, SecretFieldValue y) => x.IsSet == y.IsSet,
            (BytesFieldValue x, BytesFieldValue y) => string.Equals(x.Base64, y.Base64, StringComparison.Ordinal),
            (JsonFieldValue x, JsonFieldValue y) => x.SemanticEquals(y),
            (TableFieldValue x, TableFieldValue y) => TablesEqual(x, y),
            _ => false,
        };
    }

    private static bool TablesEqual(TableFieldValue x, TableFieldValue y)
    {
        if (x.Rows.Count != y.Rows.Count)
        {
            return false;
        }

        for (var i = 0; i < x.Rows.Count; i++)
        {
            if (!string.Equals(x.Rows[i].Id, y.Rows[i].Id, StringComparison.Ordinal))
            {
                return false;
            }

            var a = x.Rows[i].Values;
            var b = y.Rows[i].Values;
            if (a.Count != b.Count)
            {
                return false;
            }

            foreach (var pair in a)
            {
                if (!b.TryGetValue(pair.Key, out var other) || !ValuesEqual(pair.Value, other))
                {
                    return false;
                }
            }
        }

        return true;
    }
}