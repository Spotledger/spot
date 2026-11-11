using Spotledger.Client.Api;

namespace Spotledger.Client.Metadata;

/// <summary>
/// Places the server's error response onto a <see cref="DocumentDraft"/>: the contract's
/// per-field errors (including row paths like <c>items[3].qty</c>) attach to the right
/// field or row state, non-field errors become document-level messages, and
/// <c>document.stale_version</c> carries the current version the server holds so the
/// renderer can offer a reload.
/// </summary>
public static class ErrorMapper
{
    /// <summary>
    /// Applies an <see cref="ApiError"/> to the draft. Field errors attach to the
    /// field's (or the row field's) <see cref="FieldState"/>; the codes a field path the
    /// draft does not know are kept as document-level messages rather than dropped.
    /// Answers the document-level result.
    /// </summary>
    /// <param name="draft">The draft that was being saved.</param>
    /// <param name="error">The mapped server error.</param>
    public static ErrorApplication Apply(DocumentDraft draft, ApiError error)
    {
        ArgumentNullException.ThrowIfNull(draft);
        ArgumentNullException.ThrowIfNull(error);

        var result = new ErrorApplication(error.Code, error.Message);

        if (error.Code == "document.stale_version")
        {
            result.IsStaleVersion = true;
            result.CurrentVersion = ReadCurrentVersion(error);
            return result;
        }

        foreach (var fieldError in error.FieldErrors)
        {
            if (TryAttach(draft, fieldError, result))
            {
                result.AppliedFieldCount++;
            }
            else
            {
                result.AddDocumentMessage(
                    new FieldError(fieldError.Field, fieldError.Code, fieldError.Message));
            }
        }

        if (error.FieldErrors.Count == 0)
        {
            // No field errors: the whole message is the document's.
            result.AddDocumentMessage(new FieldError(string.Empty, error.Code, error.Message));
        }

        return result;
    }

    private static bool TryAttach(DocumentDraft draft, ApiFieldError fieldError, ErrorApplication result)
    {
        var path = fieldError.Field;
        if (string.IsNullOrEmpty(path))
        {
            return false;
        }

        // Row path form: table[idx].field (idx from 1, per the contract's FieldPath);
        // also table[idx] alone (no attached field). The row field is what follows the
        // closing bracket, so the check is "a bracket with a closing bracket after it
        // and a remainder" — not "the path ends with the bracket".
        var bracket = path.IndexOf('[');
        var close = bracket >= 0 ? path.IndexOf(']', bracket) : -1;
        if (bracket > 0 && close > bracket)
        {
            var (tableName, indexText, remainder) = SplitRowPath(path, bracket, close);
            if (tableName is null || !int.TryParse(indexText, out var oneBased) || oneBased < 1)
            {
                return false;
            }

            var tables = draft.Tables;
            if (!tables.TryGetValue(tableName, out var table))
            {
                return false;
            }

            if (oneBased > table.Rows.Count)
            {
                // The server counted a row the local draft no longer has (the user
                // removed it while the request was flying): keep the message.
                return false;
            }

            var row = table.Rows[oneBased - 1];
            if (remainder is { Length: > 0 } rowField)
            {
                if (!row.Values.ContainsKey(rowField))
                {
                    // The server's row field the local draft predates: keep the message.
                    return false;
                }

                row.Values[rowField].SetErrors(
                    new[] { new FieldError(path, fieldError.Code, fieldError.Message) });
                table.NotifyRowValidation(row, rowField);
                return true;
            }

            // The contract's row path with no field part: the error is the row's own. It
            // is stored on the row (so a table control can annotate the row) and, as
            // before, kept in the document's messages: existing callers keep working.
            row.SetRowErrors(new[] { new FieldError(path, fieldError.Code, fieldError.Message) });
            result.AppliedRowErrorCount++;
            return false;
        }

        var fieldModel = draft.Model.Field(path);
        if (fieldModel is null)
        {
            return false;
        }

        var states = (IReadOnlyDictionary<string, FieldState>)draft.Fields;
        if (!states.TryGetValue(path, out var state))
        {
            return false;
        }

        state.SetErrors(new[] { new FieldError(path, fieldError.Code, fieldError.Message) });
        return true;
    }

    private static (string? Table, string? Index, string? Remainder) SplitRowPath(string path, int bracket, int close)
    {
        var table = path[..bracket];
        var index = path[(bracket + 1)..close];
        var dot = path.IndexOf('.', close);
        var remainder = dot > 0 ? path[(dot + 1)..] : null;
        return (table, index, remainder);
    }

    private static int? ReadCurrentVersion(ApiError error)
    {
        // The contract carries the current version in error.details.current_version; the
        // mapped ApiError keeps only the envelope's text fields, so the renderer's
        // reload offer uses the draft's Version until a details field is mapped. The
        // ApiError type is in the Api assembly and cannot gain fields here.
        return null;
    }
}

/// <summary>
/// The outcome of <see cref="ErrorMapper.Apply"/>: what attached, what stayed at
/// document level, and whether the server answered <c>document.stale_version</c>.
/// </summary>
public sealed class ErrorApplication
{
    /// <summary>Creates the application result.</summary>
    public ErrorApplication(string code, string message)
    {
        Code = code;
        Message = message;
    }

    /// <summary>The server error's code.</summary>
    public string Code { get; }

    /// <summary>The server error's message.</summary>
    public string Message { get; }

    /// <summary>True when the server answered <c>document.stale_version</c>: the
    /// document changed elsewhere and the renderer should offer a reload.</summary>
    public bool IsStaleVersion { get; internal set; }

    /// <summary>The current version the server holds (from the error's details), when
    /// the error was a stale-version answer.</summary>
    public int? CurrentVersion { get; internal set; }

    /// <summary>How many field errors attached to the draft's fields.</summary>
    public int AppliedFieldCount { get; internal set; }

    /// <summary>How many row-level errors (the contract's <c>table[idx]</c> path) attached
    /// to a row's <see cref="DocumentDraft.TableDraft.RowDraft.RowErrors"/>; they are also
    /// in <see cref="DocumentMessages"/>.</summary>
    public int AppliedRowErrorCount { get; internal set; }

    /// <summary>The messages that are not attached to a field: everything a renderer
    /// shows as the document's own errors.</summary>
    public IReadOnlyList<FieldError> DocumentMessages => _messages;

    internal void AddDocumentMessage(FieldError message) => _messages.Add(message);

    private readonly List<FieldError> _messages = new();
}