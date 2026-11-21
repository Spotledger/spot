using System.Text;

namespace Spotledger.Client.ChildTable;

/// <summary>Reads clipboard text as a grid of cells: tab-separated columns, line-separated
/// rows, and the quoting spreadsheets add (a cell in double quotes may hold tabs, line breaks
/// and doubled quotes). One trailing empty line is dropped; any other empty line is a row of one
/// empty cell.</summary>
public static class ChildTableTsv
{
    /// <summary>Splits clipboard text into rows of cells.</summary>
    public static List<string[]> Parse(string text)
    {
        ArgumentNullException.ThrowIfNull(text);
        var rows = new List<string[]>();
        var row = new List<string>();
        var cell = new StringBuilder();
        var i = 0;
        var endedWithNewline = false;
        while (i < text.Length)
        {
            endedWithNewline = false;
            var ch = text[i];
            if (ch == '"' && cell.Length == 0)
            {
                // A quoted cell runs to the closing quote; "" inside is one quote.
                i++;
                while (i < text.Length)
                {
                    if (text[i] == '"')
                    {
                        if (i + 1 < text.Length && text[i + 1] == '"')
                        {
                            cell.Append('"');
                            i += 2;
                            continue;
                        }

                        i++;
                        break;
                    }

                    cell.Append(text[i]);
                    i++;
                }

                continue;
            }

            if (ch == '\t')
            {
                row.Add(cell.ToString());
                cell.Clear();
                i++;
                continue;
            }

            if (ch is '\r' or '\n')
            {
                row.Add(cell.ToString());
                cell.Clear();
                rows.Add([.. row]);
                row.Clear();
                if (ch == '\r' && i + 1 < text.Length && text[i + 1] == '\n')
                {
                    i++;
                }

                i++;
                endedWithNewline = true;
                continue;
            }

            cell.Append(ch);
            i++;
        }

        if (!endedWithNewline && (cell.Length > 0 || row.Count > 0 || text.Length > 0))
        {
            row.Add(cell.ToString());
            rows.Add([.. row]);
        }

        return rows;
    }
}
