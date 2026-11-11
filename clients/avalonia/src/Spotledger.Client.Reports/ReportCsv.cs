using System.Text;

namespace Spotledger.Client.Reports;

/// <summary>
/// CSV and TSV for a report result. Quoting follows RFC 4180 (a cell with a comma, a quote or a line break
/// is quoted, quotes are doubled, rows end CRLF) and the formula-injection hardening is the server's: a text
/// cell that starts with <c>=</c>, <c>+</c>, <c>-</c>, <c>@</c>, a tab or a carriage return is prefixed with
/// <c>'</c>. A number column's cell is left alone only when it really is a plain number, so a hostile value
/// in a decimal column is hardened too. Output is written to a stream a row at a time: a large result never
/// becomes one string.
/// </summary>
public static class ReportCsv
{
    private const int FlushEveryRows = 512;

    private static readonly UTF8Encoding Utf8 = new(encoderShouldEmitUTF8Identifier: false);

    /// <summary>Prefixes a leading formula character with a quote so a spreadsheet reads the cell as text.</summary>
    public static string Harden(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        return text.Length > 0 && text[0] is '=' or '+' or '-' or '@' or '\t' or '\r' ? "'" + text : text;
    }

    /// <summary>The CSV form of one cell (hardened, then quoted when needed).</summary>
    public static string Cell(string? text, bool numericColumn)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        var safe = numericColumn && ExactNumber.TryParse(text, out _) ? text : Harden(text);
        return NeedsQuotes(safe) ? "\"" + safe.Replace("\"", "\"\"", StringComparison.Ordinal) + "\"" : safe;
    }

    /// <summary>Writes the header and every row to <paramref name="destination"/> as UTF-8 CSV (no byte-order mark).
    /// The stream stays open. Returns the number of data rows written.</summary>
    public static async Task<int> WriteAsync(Stream destination, IReadOnlyList<ReportColumn> columns, IEnumerable<string?[]> rows, CancellationToken cancellationToken = default)
    {
        ArgumentNullException.ThrowIfNull(destination);
        ArgumentNullException.ThrowIfNull(columns);
        ArgumentNullException.ThrowIfNull(rows);
        var writer = new StreamWriter(destination, Utf8, 32 * 1024, leaveOpen: true);
        await using (writer.ConfigureAwait(false))
        {
            var line = new StringBuilder();
            AppendRow(line, columns.Select(c => (string?)c.Label), columns, header: true);
            await writer.WriteAsync(line.ToString().AsMemory(), cancellationToken).ConfigureAwait(false);
            var count = 0;
            foreach (var row in rows)
            {
                cancellationToken.ThrowIfCancellationRequested();
                line.Clear();
                AppendRow(line, row, columns, header: false);
                await writer.WriteAsync(line.ToString().AsMemory(), cancellationToken).ConfigureAwait(false);
                if (++count % FlushEveryRows == 0)
                {
                    await writer.FlushAsync(cancellationToken).ConfigureAwait(false);
                }
            }

            await writer.FlushAsync(cancellationToken).ConfigureAwait(false);
            return count;
        }
    }

    /// <summary>The selection as tab-separated text for the clipboard: a tab or line break inside a cell becomes a
    /// space, and a cell is hardened exactly as in CSV (a pasted cell never starts a formula).</summary>
    public static string ToTsv(IReadOnlyList<ReportColumn> columns, IEnumerable<string?[]> rows, bool includeHeader = false)
    {
        ArgumentNullException.ThrowIfNull(columns);
        ArgumentNullException.ThrowIfNull(rows);
        var builder = new StringBuilder();
        if (includeHeader)
        {
            builder.Append(string.Join('\t', columns.Select(c => TsvCell(c.Label, false)))).Append('\n');
        }

        foreach (var row in rows)
        {
            for (var c = 0; c < columns.Count; c++)
            {
                if (c > 0)
                {
                    builder.Append('\t');
                }

                builder.Append(TsvCell(c < row.Length ? row[c] : null, columns[c].IsNumeric));
            }

            builder.Append('\n');
        }

        return builder.ToString();
    }

    private static string TsvCell(string? text, bool numericColumn)
    {
        if (string.IsNullOrEmpty(text))
        {
            return string.Empty;
        }

        var safe = numericColumn && ExactNumber.TryParse(text, out _) ? text : Harden(text);
        return safe.Replace('\t', ' ').Replace('\r', ' ').Replace('\n', ' ');
    }

    private static void AppendRow(StringBuilder line, IEnumerable<string?> cells, IReadOnlyList<ReportColumn> columns, bool header)
    {
        var index = 0;
        foreach (var cell in cells)
        {
            if (index > 0)
            {
                line.Append(',');
            }

            line.Append(Cell(cell, !header && index < columns.Count && columns[index].IsNumeric));
            index++;
        }

        line.Append("\r\n");
    }

    private static bool NeedsQuotes(string text) => text.AsSpan().IndexOfAny(",\"\r\n") >= 0;
}
