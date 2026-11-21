using Spotledger.Client.Api;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 3 (WP-UI-07.T4): row-level errors. The contract's <c>table[idx]</c> error
/// (a row error with no field part) is stored on the row in <c>RowErrors</c> — where the
/// table control can annotate it — and still reported in
/// <see cref="ErrorApplication.DocumentMessages"/>, so existing callers keep working. A
/// <c>ValidationChanged</c> notification names the table and the row's key (never a
/// position), and fires when errors are applied or cleared.
/// </summary>
public sealed class RowValidationTests
{
    [Fact]
    public void RowLevelServerErrorLandsOnTheRowAndStaysInTheDocumentMessages()
    {
        var draft = TestModels.LoadedInvoice();
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        var result = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[2]", "document.validation_failed", "Check this row.")));

        // Stored on the row ...
        var row = draft.Table("taxes").Rows[1];
        var stored = Assert.Single(row.RowErrors);
        Assert.Equal("taxes[2]", stored.Field);
        Assert.Equal("document.validation_failed", stored.Code);
        Assert.Equal("Check this row.", stored.Message);
        // ... and still reported as a document message (existing callers keep working).
        Assert.Equal("taxes[2]", Assert.Single(result.DocumentMessages).Field);
        Assert.Equal(0, result.AppliedFieldCount);
        Assert.Equal(1, result.AppliedRowErrorCount);
        // The cell errors stay untouched.
        Assert.Empty(row.Values["qty"].Errors);

        var change = Assert.Single(events);
        Assert.Equal("taxes", change.Table);
        Assert.Equal("row-0002", change.RowKey);
        Assert.Null(change.Field);
        Assert.Equal(1, change.Index);
        Assert.Equal("Check this row.", Assert.Single(change.Errors).Message);
    }

    [Fact]
    public void CellErrorRaisesValidationChangedWithTheField()
    {
        var draft = TestModels.LoadedInvoice();
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        var result = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[3].qty", "document.scale_exceeded", "Too many decimals.")));

        Assert.Equal(1, result.AppliedFieldCount);
        Assert.Equal(0, result.AppliedRowErrorCount);
        var change = Assert.Single(events);
        Assert.Equal("taxes", change.Table);
        Assert.Equal("row-0003", change.RowKey);
        Assert.Equal("qty", change.Field);
        Assert.Equal("document.scale_exceeded", Assert.Single(change.Errors).Code);
    }

    [Fact]
    public void ACellEditClearsThatCellsServerErrorAndReportsTheClear()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        _ = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[1].qty", "document.scale_exceeded", "Too many decimals.")));
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        table.Rows[0].SetValue("qty", new DecimalFieldValue(12.5m));

        Assert.Empty(table.Rows[0].Values["qty"].Errors);
        var change = Assert.Single(events);
        Assert.Equal("taxes", change.Table);
        Assert.Equal("row-0001", change.RowKey);
        Assert.Equal("qty", change.Field);
        Assert.Empty(change.Errors);
    }

    [Fact]
    public void ClearingRowErrorsReportsTheClear()
    {
        var draft = TestModels.LoadedInvoice();
        var row = draft.Table("taxes").Rows[0];
        _ = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[1]", "document.validation_failed", "Check this row.")));
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        row.ClearRowErrors();

        Assert.Empty(row.RowErrors);
        var change = Assert.Single(events);
        Assert.Null(change.Field);
        Assert.Empty(change.Errors);
    }

    [Fact]
    public void AStructuralChangeReKeysNothingBecauseErrorsRideTheRow()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        _ = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[2]", "document.validation_failed", "Check this row.")));
        _ = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[2].qty", "document.scale_exceeded", "Too many decimals.")));
        var events = new List<RowValidationChangedEventArgs>();
        draft.ValidationChanged += (_, change) => events.Add(change);

        table.MoveRow(1, 0);
        table.RemoveRowAt(2);

        // The errors still belong to row-0002, now at the top: no re-keying, no events.
        Assert.Empty(events);
        Assert.Equal("row-0002", table.Rows[0].Id);
        Assert.Equal("taxes[2]", Assert.Single(table.Rows[0].RowErrors).Field);
        Assert.Equal("document.scale_exceeded", Assert.Single(table.Rows[0].Values["qty"].Errors).Code);
    }

    [Fact]
    public void UndoingADeleteRestoresTheRowsErrors()
    {
        var draft = TestModels.LoadedInvoice();
        var table = draft.Table("taxes");
        _ = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[2]", "document.validation_failed", "Check this row.")));

        table.RemoveRowAt(1);
        Assert.True(table.UndoRow());

        Assert.Equal("Check this row.", Assert.Single(table.Rows[1].RowErrors).Message);
    }

    [Fact]
    public void ErrorsCarriedByARowTheDraftNoLongerHasStayADocumentMessage()
    {
        var draft = TestModels.LoadedInvoice();
        var result = ErrorMapper.Apply(draft, Error(
            new ApiFieldError("taxes[9]", "document.validation_failed", "Row 9 is wrong.")));

        Assert.Equal(0, result.AppliedRowErrorCount);
        Assert.Equal("taxes[9]", Assert.Single(result.DocumentMessages).Field);
    }

    private static ApiError Error(params ApiFieldError[] fieldErrors) => new()
    {
        StatusCode = 422,
        Code = "document.validation_failed",
        Message = "Validation failed",
        FieldErrors = fieldErrors,
    };
}
