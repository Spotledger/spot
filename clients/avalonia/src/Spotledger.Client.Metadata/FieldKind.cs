namespace Spotledger.Client.Metadata;

/// <summary>
/// The field kinds the client runtime understands (ADR-068 §2/§6): a closed enum
/// mirroring the contract's value encodings, plus <see cref="Unknown"/> so a newer
/// server naming a kind this client predates degrades to a kept-but-uneditable field
/// instead of crashing it. <see cref="Table"/> covers both <c>table</c> and
/// <c>table_multiselect</c>; the row model's link field distinguishes the multi-pick.
/// The two numeric members carry <c>Number</c> (CA1720) so no identifier shadows a
/// type name.
/// </summary>
public enum FieldKind
{
    /// <summary>A true/false value.</summary>
    Bool,

    /// <summary>A 64-bit signed integer, carried as <see cref="long"/> (the contract
    /// encodes large integers as strings).</summary>
    IntegerNumber,

    /// <summary>An exact decimal with the field's scale and precision; money and
    /// quantities are always this, never floating point (ADR-12).</summary>
    DecimalNumber,

    /// <summary>A single-line text value.</summary>
    Text,

    /// <summary>A multi-line text value, encoded like text.</summary>
    LongText,

    /// <summary>A calendar date (<c>YYYY-MM-DD</c>).</summary>
    Date,

    /// <summary>A wall-clock time (<c>HH:MM:SS</c>).</summary>
    Time,

    /// <summary>An instant in time, held as a <see cref="DateTimeOffset"/>.</summary>
    Datetime,

    /// <summary>A reference to another document: an id plus, when known, display text.</summary>
    Link,

    /// <summary>A link whose target DocType is named by another field.</summary>
    DynamicLink,

    /// <summary>One of the field's listed options.</summary>
    Select,

    /// <summary>Arbitrary JSON data kept as a JSON text.</summary>
    Json,

    /// <summary>Base64 bytes, kept as their encoded text.</summary>
    Bytes,

    /// <summary>A reference to a file record (WP-PLT-10), never the bytes.</summary>
    Attach,

    /// <summary>A secret, never encoded in a document: only whether one is set.</summary>
    Secret,

    /// <summary>A child table (also <c>table_multiselect</c>).</summary>
    Table,

    /// <summary>A field kind a newer server knows and this client does not. The field
    /// stays visible with its raw encoding; it cannot be edited.</summary>
    Unknown,
}

/// <summary>
/// Extensions over <see cref="FieldKind"/>: mapping the contract's field-type encoding
/// (and its string name) in and out, and the per-kind default the metadata may omit.
/// </summary>
public static class FieldKindExtensions
{
    /// <summary>
    /// Maps the contract's field-type encoding into a <see cref="FieldKind"/>, folding
    /// <c>long_text</c> into <see cref="FieldKind.LongText"/> and both table kinds into
    /// <see cref="FieldKind.Table"/>.
    /// </summary>
    public static FieldKind FromEncoding(FieldTypeEncoding encoding) => encoding switch
    {
        FieldTypeEncoding.Bool => FieldKind.Bool,
        FieldTypeEncoding.IntegerNumber => FieldKind.IntegerNumber,
        FieldTypeEncoding.DecimalText => FieldKind.DecimalNumber,
        FieldTypeEncoding.Text => FieldKind.Text,
        FieldTypeEncoding.LongText => FieldKind.LongText,
        FieldTypeEncoding.Date => FieldKind.Date,
        FieldTypeEncoding.Time => FieldKind.Time,
        FieldTypeEncoding.Datetime => FieldKind.Datetime,
        FieldTypeEncoding.Link => FieldKind.Link,
        FieldTypeEncoding.Json => FieldKind.Json,
        FieldTypeEncoding.Bytes => FieldKind.Bytes,
        FieldTypeEncoding.Attach => FieldKind.Attach,
        FieldTypeEncoding.Secret => FieldKind.Secret,
        FieldTypeEncoding.Table => FieldKind.Table,
        FieldTypeEncoding.TableMultiselect => FieldKind.Table,
        _ => FieldKind.Text,
    };

    /// <summary>
    /// A field whose kind arrived as a string the client does not know (a newer
    /// server's addition) becomes <see cref="FieldKind.Unknown"/>, keeping the name:
    /// never a crash, never a misinterpretation.
    /// </summary>
    public static FieldKind FromEncodingName(string encoding) => encoding switch
    {
        "bool" => FieldKind.Bool,
        "int" => FieldKind.IntegerNumber,
        "decimal" => FieldKind.DecimalNumber,
        "text" => FieldKind.Text,
        "long_text" => FieldKind.LongText,
        "date" => FieldKind.Date,
        "time" => FieldKind.Time,
        "datetime" => FieldKind.Datetime,
        "link" => FieldKind.Link,
        "json" => FieldKind.Json,
        "bytes" => FieldKind.Bytes,
        "attach" => FieldKind.Attach,
        "secret" => FieldKind.Secret,
        "table" => FieldKind.Table,
        "table_multiselect" => FieldKind.Table,
        _ => FieldKind.Unknown,
    };

    /// <summary>The contract encoding name of the kind, for tests and diagnostics.</summary>
    public static string ToEncodingName(this FieldKind kind) => kind switch
    {
        FieldKind.Bool => "bool",
        FieldKind.IntegerNumber => "int",
        FieldKind.DecimalNumber => "decimal",
        FieldKind.Text => "text",
        FieldKind.LongText => "long_text",
        FieldKind.Date => "date",
        FieldKind.Time => "time",
        FieldKind.Datetime => "datetime",
        FieldKind.Link => "link",
        FieldKind.DynamicLink => "link",
        FieldKind.Select => "text",
        FieldKind.Json => "json",
        FieldKind.Bytes => "bytes",
        FieldKind.Attach => "attach",
        FieldKind.Secret => "secret",
        FieldKind.Table => "table",
        FieldKind.Unknown => "text",
        _ => "text",
    };

    /// <summary>
    /// The value a field starts with when its metadata carries no default (ADR-068 §6):
    /// the kind's zero-shape — numeric zero for the numbers, empty text for the texts,
    /// and no value at all for the kinds nothing is known about yet (a required check
    /// must fire against these).
    /// </summary>
    public static FieldValue EmptyValue(this FieldKind kind) => kind switch
    {
        FieldKind.Bool => new BoolFieldValue(false),
        FieldKind.IntegerNumber => FieldValue.Zero,
        FieldKind.DecimalNumber => new DecimalFieldValue(ExactDecimal.Zero),
        FieldKind.Text or FieldKind.LongText => new TextFieldValue(string.Empty),
        FieldKind.Select => new SelectFieldValue(string.Empty),
        FieldKind.Table => new TableFieldValue(Array.Empty<TableRowValue>(), null),
        _ => FieldValue.Null,
    };
}

/// <summary>
/// The contract's field-type encodings, restated locally with string names: the
/// generated enum is the transport's, this one is the model's.
/// </summary>
public enum FieldTypeEncoding
{
    /// <summary>Encoded as a boolean.</summary>
    Bool,

    /// <summary>Encoded as an integer (large integers as strings on the wire).</summary>
    IntegerNumber,

    /// <summary>Encoded as a decimal string.</summary>
    DecimalText,

    /// <summary>Encoded as text.</summary>
    Text,

    /// <summary>Encoded as text (multi-line).</summary>
    LongText,

    /// <summary>Encoded as a <c>YYYY-MM-DD</c> string.</summary>
    Date,

    /// <summary>Encoded as an <c>HH:MM:SS</c> string.</summary>
    Time,

    /// <summary>Encoded as an RFC 3339 string.</summary>
    Datetime,

    /// <summary>Encoded as a document id string.</summary>
    Link,

    /// <summary>Encoded as embedded JSON.</summary>
    Json,

    /// <summary>Encoded as standard base64 text.</summary>
    Bytes,

    /// <summary>Encoded as a file record id string.</summary>
    Attach,

    /// <summary>Never encoded in a document: only whether one is set.</summary>
    Secret,

    /// <summary>Encoded as an array of row objects.</summary>
    Table,

    /// <summary>Encoded as an array of row objects (the multi-pick).</summary>
    TableMultiselect,
}

/// <summary>Parse helpers for the <see cref="FieldTypeEncoding"/> names.</summary>
public static class FieldTypeEncodingName
{
    /// <summary>Maps a wire name to its encoding; <c>null</c> when the name is unknown
    /// (a newer server's addition).</summary>
    public static FieldTypeEncoding? TryParse(string name) => name switch
    {
        "bool" => FieldTypeEncoding.Bool,
        "int" => FieldTypeEncoding.IntegerNumber,
        "decimal" => FieldTypeEncoding.DecimalText,
        "text" => FieldTypeEncoding.Text,
        "long_text" => FieldTypeEncoding.LongText,
        "date" => FieldTypeEncoding.Date,
        "time" => FieldTypeEncoding.Time,
        "datetime" => FieldTypeEncoding.Datetime,
        "link" => FieldTypeEncoding.Link,
        "json" => FieldTypeEncoding.Json,
        "bytes" => FieldTypeEncoding.Bytes,
        "attach" => FieldTypeEncoding.Attach,
        "secret" => FieldTypeEncoding.Secret,
        "table" => FieldTypeEncoding.Table,
        "table_multiselect" => FieldTypeEncoding.TableMultiselect,
        _ => null,
    };
}