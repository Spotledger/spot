using System.Text;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;

namespace Spotledger.Client.Lists;

public sealed partial class ListViewModel
{
    /// <summary>The summary of the last bulk delete, or null.</summary>
    public BulkDeleteSummary? LastDelete { get; private set; }

    /// <summary>Deletes the selected documents after a confirmation. Each delete is its own request at the
    /// loaded version; a document the server refuses (still referenced, posted, changed) is listed, with
    /// the documents that refer to it when the server says which.</summary>
    public async Task<BulkDeleteSummary?> DeleteSelectedAsync(CancellationToken cancellationToken = default)
    {
        if (!CanDelete || _source is null)
        {
            return null;
        }

        var ids = _selected.Take(_options.MaxBulkDelete).ToArray();
        var count = ids.Length;
        var noun = count == 1 ? "document" : "documents";
        if (!await _shell.ConfirmAsync("Delete", $"Delete {count} {noun}? This cannot be undone.", "Delete").ConfigureAwait(true))
        {
            return null;
        }

        IsBusy = true;
        RaiseState();
        try
        {
            var failures = new List<DeleteFailure>();
            var deleted = 0;
            foreach (var id in ids)
            {
                cancellationToken.ThrowIfCancellationRequested();
                var display = _source.DisplayTextOf(id) ?? id;
                if (_source.VersionOf(id) is not { } version)
                {
                    failures.Add(new DeleteFailure(id, display, new ApiError { Code = "list.not_loaded", Message = "The document is no longer in the list. Refresh and try again." }));
                    continue;
                }

                try
                {
                    await _gateway.DeleteAsync(DocType, id, version, cancellationToken).ConfigureAwait(true);
                    deleted++;
                }
                catch (ApiRequestException error)
                {
                    failures.Add(new DeleteFailure(id, display, error.Error));
                }
            }

            var summary = new BulkDeleteSummary(count, deleted, failures);
            LastDelete = summary;
            _selected.Clear();
            _source.Refresh();
            Message = failures.Count == 0
                ? $"Deleted {deleted} {(deleted == 1 ? "document" : "documents")}."
                : $"Deleted {deleted} of {count}. {failures.Count} could not be deleted.";
            if (failures.Count > 0)
            {
                await _shell.ShowReferrersAsync("Cannot delete: " + failures.Count + " not deleted", Summarise(failures)).ConfigureAwait(true);
            }

            return summary;
        }
        finally
        {
            IsBusy = false;
            RaiseState();
        }
    }

    private static List<Referrer> Summarise(IReadOnlyList<DeleteFailure> failures)
    {
        var list = new List<Referrer>();
        foreach (var failure in failures)
        {
            if (failure.Error.FieldErrors.Count == 0)
            {
                list.Add(new Referrer(failure.Display, failure.Error.Message));
                continue;
            }

            foreach (var field in failure.Error.FieldErrors)
            {
                list.Add(new Referrer(failure.Display + ": " + (string.IsNullOrEmpty(field.Field) ? field.Message : field.Field),
                    string.IsNullOrEmpty(field.Field) ? null : field.Message));
            }
        }

        return list;
    }

    /// <summary>True when an export can run (a sink is configured and the list is ready).</summary>
    public bool CanExport => Phase == ListPhase.Ready && _options.ExportSink is not null && _source is not null;

    /// <summary>Exports the current query as CSV: through the bulk export endpoint when the gateway offers it,
    /// otherwise from the rows loaded in the list, with a message that says how many.</summary>
    public async Task<int> ExportAsync(CancellationToken cancellationToken = default)
    {
        if (!CanExport || _source is null || _columnSet is null || _options.ExportSink is not { } sink)
        {
            return 0;
        }

        var name = DocType + ".csv";
        string csv;
        int rows;
        if (_gateway.SupportsBulkExport)
        {
            try
            {
                csv = await _gateway.ExportCsvAsync(DocType, _columnSet.FieldNames, _source.Filter, _source.Order, _options.ExportRowLimit, cancellationToken).ConfigureAwait(true);
            }
            catch (ApiRequestException error)
            {
                Message = error.Error.Message;
                RaiseState();
                return 0;
            }

            rows = Math.Max(0, csv.Count(c => c == '\n') - 1);
            Message = $"Exported {rows:N0} rows.";
        }
        else
        {
            var loaded = _source.CachedRows();
            csv = ToCsv(_columnSet, loaded);
            rows = loaded.Count;
            var total = _source.ExactTotal;
            Message = total is { } all && all <= rows
                ? $"Exported {rows:N0} rows."
                : $"Exported the {rows:N0} rows loaded in the list. The server's bulk export is not reachable from this client, so only loaded rows are exported.";
        }

        await sink.SaveCsvAsync(name, csv).ConfigureAwait(true);
        RaiseState();
        return rows;
    }

    /// <summary>CSV of display rows. A text cell that starts with =, +, -, @, tab or CR is prefixed with an
    /// apostrophe so a spreadsheet does not run it (as the server's export does).</summary>
    internal static string ToCsv(ListColumnSet columns, IReadOnlyList<Spotledger.Client.Grid.GridRow> rows)
    {
        var builder = new StringBuilder();
        builder.Append(string.Join(',', columns.Columns.Select(c => Quote(c.Model.Header)))).Append("\r\n");
        foreach (var row in rows)
        {
            builder.Append(string.Join(',', row.Cells.Select(c => Quote(c.Text)))).Append("\r\n");
        }

        return builder.ToString();
    }

    private static string Quote(string text)
    {
        if (text.Length > 0 && text[0] is '=' or '+' or '-' or '@' or '\t' or '\r')
        {
            text = "'" + text;
        }

        return text.IndexOfAny([',', '"', '\r', '\n']) >= 0 ? "\"" + text.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"" : text;
    }
}
