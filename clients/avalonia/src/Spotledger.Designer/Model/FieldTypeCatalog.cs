using Spotledger.Client.Metadata;

namespace Spotledger.Client.Designer;

/// <summary>
/// One entry of the field-type palette: the server's type name, what a person calls it, the
/// client control kind it renders as, and what the inspector must ask for before the field can
/// be added.
/// </summary>
public sealed record FieldTypeInfo(
    string Type,
    string Title,
    string Description,
    FieldKind Kind,
    string Group,
    bool NeedsOptions = false,
    bool NeedsTarget = false,
    bool IsChildTable = false)
{
    /// <summary>The palette's title, for pickers that show the entry itself.</summary>
    public override string ToString() => Title;
}

/// <summary>
/// The palette: every field type the client's control library can render (the
/// <see cref="FieldKind"/> values of <c>Spotledger.Client.Metadata</c> that a designer may add),
/// paired with the server's own type name. A kind the controls know but this palette does not
/// offer (an unknown kind from a newer server) simply has no entry: the designer never invents a
/// server type.
/// </summary>
public static class FieldTypeCatalog
{
    /// <summary>Every addable type, grouped as the palette shows them.</summary>
    public static IReadOnlyList<FieldTypeInfo> All { get; } = new[]
    {
        new FieldTypeInfo("text", "Text", "A single line of text", FieldKind.Text, "Text"),
        new FieldTypeInfo("long_text", "Long text", "A paragraph of text", FieldKind.LongText, "Text"),
        new FieldTypeInfo("int", "Whole number", "A number with no decimals", FieldKind.IntegerNumber, "Numbers"),
        new FieldTypeInfo("decimal", "Amount", "An exact decimal (money, quantity, rate)", FieldKind.DecimalNumber, "Numbers"),
        new FieldTypeInfo("date", "Date", "A calendar date", FieldKind.Date, "Date and time"),
        new FieldTypeInfo("time", "Time", "A time of day", FieldKind.Time, "Date and time"),
        new FieldTypeInfo("datetime", "Date and time", "A moment in time", FieldKind.Datetime, "Date and time"),
        new FieldTypeInfo("bool", "Yes / no", "A true or false switch", FieldKind.Bool, "Choice"),
        new FieldTypeInfo("select", "Choice list", "One value from a list you write", FieldKind.Select, "Choice", NeedsOptions: true),
        new FieldTypeInfo("link", "Link", "A reference to a record of another DocType", FieldKind.Link, "Reference", NeedsTarget: true),
        new FieldTypeInfo("dynamic_link", "Flexible link", "A link whose target another field names", FieldKind.DynamicLink, "Reference", NeedsTarget: false),
        new FieldTypeInfo("table", "Child table", "A grid of rows of a child DocType", FieldKind.Table, "Reference", IsChildTable: true),
        new FieldTypeInfo("attach", "Attachment", "A file attached to the record", FieldKind.Attach, "Files and data"),
        new FieldTypeInfo("json", "Structured data", "A JSON document", FieldKind.Json, "Files and data"),
        new FieldTypeInfo("bytes", "Binary data", "Encoded bytes", FieldKind.Bytes, "Files and data"),
        new FieldTypeInfo("secret", "Secret", "A password or key held by the secrets service", FieldKind.Secret, "Files and data"),
    };

    /// <summary>The palette's groups, in order, each with its entries.</summary>
    public static IReadOnlyList<(string Group, IReadOnlyList<FieldTypeInfo> Fields)> Groups()
    {
        var order = new[] { "Text", "Numbers", "Date and time", "Choice", "Reference", "Files and data" };
        return order
            .Select(g => (g, (IReadOnlyList<FieldTypeInfo>)All.Where(f => string.Equals(f.Group, g, StringComparison.Ordinal)).ToList()))
            .Where(pair => pair.Item2.Count > 0)
            .ToList();
    }

    /// <summary>The entry for a server type name, or null.</summary>
    public static FieldTypeInfo? Find(string type) =>
        All.FirstOrDefault(f => string.Equals(f.Type, type, StringComparison.Ordinal));

    /// <summary>The palette title for a type name; the name itself when the palette has no entry.</summary>
    public static string TitleOf(string type) => Find(type)?.Title ?? type;

    /// <summary>
    /// The client control kind for a server type name. An unrecognised name degrades to
    /// <see cref="FieldKind.Unknown"/> (shown, not editable) instead of guessing.
    /// </summary>
    public static FieldKind KindOf(string type) => Find(type)?.Kind ?? FieldKind.Unknown;

    /// <summary>The select options a choice-list field added from the palette starts with.</summary>
    public static IReadOnlyList<string> DefaultOptions { get; } = new[] { "First", "Second", "Third" };
}

/// <summary>
/// The client half of `spot-nocode-model`'s naming rules: a label becomes a safe storage name,
/// and a name that is taken gets a number. The server still validates every name it receives;
/// this only shows the designer the name it is about to use (the crate's own words) and keeps
/// the UI honest when the user clears the name field.
/// </summary>
public static class DesignerNaming
{
    /// <summary>Longest storage name, in bytes; the server's limit.</summary>
    public const int MaxNameBytes = 63;

    /// <summary>The prefix a field of an already-existing DocType must carry.</summary>
    public const string CustomPrefix = "custom_";

    /// <summary>Whether a name is a storage name: <c>[a-z][a-z0-9_]*</c>, at most 63 bytes.</summary>
    public static bool IsStorageName(string? name)
    {
        if (string.IsNullOrEmpty(name) || name[0] is < 'a' or > 'z')
        {
            return false;
        }

        return name.Length <= MaxNameBytes && name.All(c => c is >= 'a' and <= 'z' or >= '0' and <= '9' or '_');
    }

    /// <summary>A label as a safe storage identifier, following the server's rules exactly.</summary>
    public static string SafeIdentifier(string label)
    {
        ArgumentNullException.ThrowIfNull(label);
        var builder = new System.Text.StringBuilder(label.Length);
        var pendingSeparator = false;
        foreach (var c in label)
        {
            if (char.IsAsciiLetterOrDigit(c))
            {
                if (pendingSeparator && builder.Length > 0)
                {
                    builder.Append('_');
                }

                pendingSeparator = false;
                builder.Append(char.ToLowerInvariant(c));
            }
            else
            {
                pendingSeparator = true;
            }
        }

        var name = builder.Length == 0 ? "f" : builder.ToString();
        if (char.IsAsciiDigit(name[0]))
        {
            name = "f" + name;
        }

        return Truncate(name);
    }

    /// <summary>
    /// A field name for a label: a plain name for a DocType being created, a
    /// <c>custom_</c>-prefixed one for a field added to an existing DocType.
    /// </summary>
    public static string FieldName(string label, bool existingDocType)
    {
        var baseName = SafeIdentifier(label);
        if (!existingDocType)
        {
            return baseName;
        }

        return baseName.StartsWith(CustomPrefix, StringComparison.Ordinal)
            ? Truncate(baseName)
            : Truncate(CustomPrefix + baseName);
    }

    /// <summary>
    /// A name that is not taken: the base, else <c>base_2</c>, <c>base_3</c>, and so on. A
    /// throwaway edit of a field keeps its own name (the caller passes it as taken otherwise).
    /// </summary>
    public static string Unique(IReadOnlyList<string> taken, string baseName)
    {
        ArgumentNullException.ThrowIfNull(taken);
        if (!taken.Contains(baseName, StringComparer.Ordinal))
        {
            return Truncate(baseName);
        }

        for (var n = 2; n < int.MaxValue; n++)
        {
            var candidate = Truncate($"{baseName}_{n}");
            if (!taken.Contains(candidate, StringComparer.Ordinal))
            {
                return candidate;
            }
        }

        return Truncate(baseName);
    }

    private static string Truncate(string name)
    {
        if (name.Length <= MaxNameBytes)
        {
            return name;
        }

        return name[..MaxNameBytes].TrimEnd('_');
    }
}
