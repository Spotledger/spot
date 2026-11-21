namespace Spotledger.Client.Metadata;

/// <summary>
/// A validation or parse error attached to one field, with the field path the
/// contract's field errors use: <c>items[3].qty</c> for a child row's field (rows
/// counted from 1), <c>customer</c> for the document's own field.
/// </summary>
/// <param name="Field">The full field path.</param>
/// <param name="Code">The dotted stable error code.</param>
/// <param name="Message">The short human-readable message.</param>
public sealed record FieldError(string Field, string Code, string Message);

/// <summary>Helpers to compose the contract's field paths.</summary>
public static class FieldPath
{
    /// <summary>The path of a field on the document itself.</summary>
    public static string OfField(string fieldName) => fieldName;

    /// <summary>The path of a field on the <paramref name="row"/>-th row (1-based) of a
    /// child table.</summary>
    public static string OfRow(string tableName, int row, string fieldName) =>
        $"{tableName}[{row}].{fieldName}";
}