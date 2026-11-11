using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;
using Xunit.Sdk;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 8's error mapping: the contract's per-field errors — document fields and
/// row paths like <c>taxes[3].qty</c> (rows counted from 1) — attach to the right field
/// or row state, paths the draft does not know stay as document-level messages (never
/// dropped), and <c>document.stale_version</c> is recognised so the renderer can offer a
/// reload.
/// </summary>
public sealed class ErrorMapperTests
{
    [Fact]
    public void FieldErrorsAttachToTheRightFieldStates()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("customer", "document.link_missing", "Customer does not exist."),
            new ApiFieldError("billing_email", "document.invalid_format", "The e-mail is not valid."),
        ]);

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(2, result.AppliedFieldCount);
        Assert.Empty(result.DocumentMessages);
        Assert.Equal("document.link_missing", SingleErrorOf(draft, "customer").Code);
        Assert.Equal("customer", SingleErrorOf(draft, "customer").Field);
        Assert.Equal("Customer does not exist.", SingleErrorOf(draft, "customer").Message);
        Assert.Equal("document.invalid_format", SingleErrorOf(draft, "billing_email").Code);
    }

    [Fact]
    public void RowErrorAttachesToTheRowFieldWithTheFullPath()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("taxes[3].qty", "document.scale_exceeded", "Too many decimals."),
        ]);

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(1, result.AppliedFieldCount);
        Assert.Empty(result.DocumentMessages);
        var rowError = SingleErrorOfRow(draft, "taxes", 3, "qty");
        Assert.Equal("document.scale_exceeded", rowError.Code);
        Assert.Equal("taxes[3].qty", rowError.Field); // the path the server sent, kept verbatim
        Assert.Equal("Too many decimals.", rowError.Message);
        // The right row: rows count from 1, so [3] is the third loaded row.
        Assert.Equal(2, IndexOfRowWithAttachedError(draft, "taxes", "qty"));
    }

    [Fact]
    public void UnknownFieldPathBecomesADocumentMessage()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("nonexistent_field", "document.invalid", "The server knows this field."),
        ]);

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(0, result.AppliedFieldCount);
        var message = Assert.Single(result.DocumentMessages);
        Assert.Equal("nonexistent_field", message.Field);
        Assert.Equal("document.invalid", message.Code);
    }

    [Fact]
    public void RowBeyondTheDraftBecomesADocumentMessage()
    {
        // The user removed row 9 while the request was flying; the server's answer
        // against it survives as a message.
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("taxes[9].qty", "document.invalid", "Row 9 is wrong."),
        ]);

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(0, result.AppliedFieldCount);
        Assert.Equal("taxes[9].qty", Assert.Single(result.DocumentMessages).Field);
    }

    [Fact]
    public void UnknownRowFieldBecomesADocumentMessageToo()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("taxes[1].nonexistent", "document.invalid", "Unknown row field."),
        ]);

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(0, result.AppliedFieldCount);
        Assert.Equal("taxes[1].nonexistent", Assert.Single(result.DocumentMessages).Field);
    }

    [Fact]
    public void ErrorWithoutFieldErrorsBecomesTheDocumentMessage()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 403, code: "document.update_denied", message: "Not allowed to edit.");

        var result = ErrorMapper.Apply(draft, error);

        Assert.Equal(0, result.AppliedFieldCount);
        var message = Assert.Single(result.DocumentMessages);
        Assert.Equal("document.update_denied", message.Code);
        Assert.Equal("Not allowed to edit.", message.Message);
        Assert.Equal("document.update_denied", result.Code);
        Assert.Equal("Not allowed to edit.", result.Message);
    }

    [Fact]
    public void StaleVersionIsRecognisedAsTheReloadSignal()
    {
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(
            status: 412,
            code: "document.stale_version",
            message: "The document changed elsewhere; reload.");

        var result = ErrorMapper.Apply(draft, error);

        Assert.True(result.IsStaleVersion);
        Assert.Equal("document.stale_version", result.Code);
        Assert.Equal(0, result.AppliedFieldCount); // no field state carries it
        Assert.Empty(result.DocumentMessages); // and it is not a document message either
        // CurrentVersion stays null on this layer: the current version the server holds
        // arrives in error.details.current_version, which the ApiError record (the Api
        // project's shape, not an owned path here) does not carry — see the task report.
        Assert.Null(result.CurrentVersion);
    }

    [Fact]
    public void MappingLeavesValuesUntouched()
    {
        // The mapper attaches errors; it never edits the draft's values or dirtiness.
        var draft = TestModels.LoadedInvoice();
        var error = ErrorOf(status: 422, code: "document.invalid", fieldErrors:
        [
            new ApiFieldError("customer", "document.link_missing", "Missing."),
        ]);

        _ = ErrorMapper.Apply(draft, error);

        Assert.Equal("CUST-0001", Assert.IsType<LinkFieldValue>(draft.Get("customer")).Id);
        Assert.False(draft.IsDirty);
    }

    private static ApiError ErrorOf(
        int status,
        string code,
        string message = "The server rejected the save.",
        IReadOnlyList<ApiFieldError>? fieldErrors = null) => new()
    {
        StatusCode = status,
        Code = code,
        Message = message,
        FieldErrors = fieldErrors ?? Array.Empty<ApiFieldError>(),
    };

    private static FieldError SingleErrorOf(DocumentDraft draft, string field) =>
        Assert.Single(draft.Fields[field].Errors);

    private static FieldError SingleErrorOfRow(
        DocumentDraft draft,
        string table,
        int oneBasedRow,
        string field) =>
        Assert.Single(draft.Table(table).Rows[oneBasedRow - 1].Values[field].Errors);

    private static int IndexOfRowWithAttachedError(DocumentDraft draft, string table, string field)
    {
        var rows = draft.Table(table).Rows;
        for (var i = 0; i < rows.Count; i++)
        {
            if (rows[i].Values.TryGetValue(field, out var state) && state.Errors.Count > 0)
            {
                return i;
            }
        }

        throw new Xunit.Sdk.XunitException($"No row of {table} carries an error for {field}.");
    }
}