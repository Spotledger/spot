using System.Text.Json;
using Spotledger.Client.Grid;

namespace Spotledger.Client.Reports;

/// <summary>One row of the catalogue: a report the caller may see.</summary>
/// <param name="Name">The report's identifier.</param>
/// <param name="Label">What people call it.</param>
/// <param name="App">The app that owns it.</param>
/// <param name="BaseDoctype">The DocType it reads.</param>
public sealed record ReportSummary(string Name, string Label, string App, string BaseDoctype);

/// <summary>The kinds of report parameter the contract knows. <see cref="Unknown"/> is a newer server's kind.</summary>
public enum ReportParameterType
{
    /// <summary>A newer server's kind: the report cannot be run from this client.</summary>
    Unknown,

    /// <summary>Free text.</summary>
    Text,

    /// <summary>A whole number.</summary>
    IntegerNumber,

    /// <summary>An exact decimal.</summary>
    DecimalNumber,

    /// <summary>A calendar date.</summary>
    Date,

    /// <summary>A reference to a document of the options' DocType.</summary>
    Link,

    /// <summary>One of a listed set of values.</summary>
    Select,
}

/// <summary>Where a <c>select</c> or <c>link</c> parameter gets its choices.</summary>
/// <param name="Values">The listed values (a select); empty for a link.</param>
/// <param name="LinkDoctype">The DocType whose candidates the lookup supplies (a link); null for a select.</param>
public sealed record ReportParameterOptions(IReadOnlyList<string> Values, string? LinkDoctype);

/// <summary>One declared parameter of a report.</summary>
/// <param name="Name">The key the run request carries the value under.</param>
/// <param name="Label">The caption.</param>
/// <param name="Type">The kind.</param>
/// <param name="Required">True when a run needs a value.</param>
/// <param name="Default">The value used when none is sent, in the contract's encoding; null when none.</param>
/// <param name="Options">The choices of a select or link.</param>
public sealed record ReportParameterDef(
    string Name,
    string Label,
    ReportParameterType Type,
    bool Required,
    JsonElement? Default,
    ReportParameterOptions? Options);

/// <summary>One column of a report as the caller may see it.</summary>
/// <param name="Name">The column's identifier.</param>
/// <param name="Label">The header.</param>
/// <param name="Type">The contract's field type name (<c>decimal</c>, <c>text</c>, <c>link</c> and so on).</param>
/// <param name="Summable">True when the description says a total of the column is meaningful.</param>
public sealed record ReportColumn(string Name, string Label, string Type, bool Summable = false)
{
    /// <summary>True for a decimal column (right-aligned, exact text, totals).</summary>
    public bool IsDecimal => string.Equals(Type, "decimal", StringComparison.Ordinal);

    /// <summary>True for any number column.</summary>
    public bool IsNumeric => IsDecimal || string.Equals(Type, "int", StringComparison.Ordinal);

    /// <summary>The alignment cells of this column use.</summary>
    public GridCellAlignment Alignment => IsNumeric ? GridCellAlignment.End : GridCellAlignment.Start;
}

/// <summary>A report as described for the caller (parameters, visible columns, row limit).</summary>
/// <param name="Name">The report's identifier.</param>
/// <param name="Label">The title.</param>
/// <param name="App">The owning app.</param>
/// <param name="BaseDoctype">The DocType it reads.</param>
/// <param name="Parameters">The declared parameters in order.</param>
/// <param name="Columns">The columns the caller may see.</param>
/// <param name="MaxRows">The largest limit a run honours.</param>
public sealed record ReportDescription(
    string Name,
    string Label,
    string App,
    string BaseDoctype,
    IReadOnlyList<ReportParameterDef> Parameters,
    IReadOnlyList<ReportColumn> Columns,
    int MaxRows);

/// <summary>A message that came with a run.</summary>
/// <param name="Level"><c>info</c> or <c>warning</c>.</param>
/// <param name="Code">The dotted code.</param>
/// <param name="Text">The server's text (plain text, never markup).</param>
public sealed record ReportMessage(string Level, string Code, string Text);

/// <summary>The answer of a run: columns, rows as exact text, and whether the limit cut it.</summary>
/// <param name="Columns">The columns of this result, index-aligned with every row.</param>
/// <param name="Rows">The rows; a cell is its exact text (decimals keep every digit) or null.</param>
/// <param name="Truncated">True when the limit cut the result.</param>
/// <param name="Messages">The messages that came with it.</param>
public sealed record ReportRunResult(
    IReadOnlyList<ReportColumn> Columns,
    IReadOnlyList<string?[]> Rows,
    bool Truncated,
    IReadOnlyList<ReportMessage> Messages);

/// <summary>Reads the contract's report bodies. A number is kept as its raw text, so a value never passes
/// through a floating-point type.</summary>
public static class ReportJson
{
    /// <summary>The code of the message that says how many columns were left out for the caller.</summary>
    public const string ColumnsRemovedCode = "report.columns_removed";

    private static readonly JsonDocumentOptions Options = new() { MaxDepth = 16 };

    /// <summary>Reads <c>{ "data": [ReportSummary] }</c>. Entries without a name are skipped.</summary>
    public static IReadOnlyList<ReportSummary> ReadSummaries(string json)
    {
        using var document = JsonDocument.Parse(json, Options);
        var result = new List<ReportSummary>();
        if (!document.RootElement.TryGetProperty("data", out var data) || data.ValueKind != JsonValueKind.Array)
        {
            return result;
        }

        foreach (var item in data.EnumerateArray())
        {
            var name = Text(item, "name");
            if (string.IsNullOrEmpty(name))
            {
                continue;
            }

            result.Add(new ReportSummary(name, Text(item, "label") is { Length: > 0 } label ? label : name, Text(item, "app") ?? string.Empty, Text(item, "base_doctype") ?? string.Empty));
        }

        return result;
    }

    /// <summary>Reads <c>{ "data": ReportDescription }</c>.</summary>
    public static ReportDescription ReadDescription(string json)
    {
        using var document = JsonDocument.Parse(json, Options);
        var root = document.RootElement.TryGetProperty("data", out var data) ? data : document.RootElement;
        var name = Text(root, "name") ?? throw new FormatException("The report description has no name.");
        var parameters = new List<ReportParameterDef>();
        if (root.TryGetProperty("parameters", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            foreach (var item in list.EnumerateArray())
            {
                parameters.Add(ReadParameter(item));
            }
        }

        var maxRows = root.TryGetProperty("max_rows", out var max) && max.ValueKind == JsonValueKind.Number && max.TryGetInt32(out var rows) && rows > 0 ? rows : 1000;
        return new ReportDescription(
            name,
            Text(root, "label") is { Length: > 0 } label ? label : name,
            Text(root, "app") ?? string.Empty,
            Text(root, "base_doctype") ?? string.Empty,
            parameters,
            ReadColumns(root),
            maxRows);
    }

    /// <summary>Reads <c>{ "data": ReportResult, "messages": [...] }</c>.</summary>
    public static ReportRunResult ReadRun(string json)
    {
        using var document = JsonDocument.Parse(json, Options);
        var root = document.RootElement;
        var data = root.TryGetProperty("data", out var d) ? d : root;
        var columns = ReadColumns(data);
        var rows = new List<string?[]>();
        if (data.TryGetProperty("rows", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            rows.Capacity = list.GetArrayLength();
            foreach (var row in list.EnumerateArray())
            {
                var cells = new string?[columns.Count];
                if (row.ValueKind == JsonValueKind.Array)
                {
                    var index = 0;
                    foreach (var cell in row.EnumerateArray())
                    {
                        if (index >= cells.Length)
                        {
                            break;
                        }

                        cells[index++] = CellText(cell);
                    }
                }

                rows.Add(cells);
            }
        }

        var truncated = data.TryGetProperty("truncated", out var t) && t.ValueKind == JsonValueKind.True;
        var messages = new List<ReportMessage>();
        if (root.TryGetProperty("messages", out var notes) && notes.ValueKind == JsonValueKind.Array)
        {
            foreach (var note in notes.EnumerateArray())
            {
                messages.Add(new ReportMessage(Text(note, "level") ?? "info", Text(note, "code") ?? string.Empty, Text(note, "message") ?? string.Empty));
            }
        }

        return new ReportRunResult(columns, rows, truncated, messages);
    }

    /// <summary>The exact text of a result cell: a string as is, a number as written, a flag as true/false.</summary>
    public static string? CellText(JsonElement cell) => cell.ValueKind switch
    {
        JsonValueKind.Null or JsonValueKind.Undefined => null,
        JsonValueKind.String => cell.GetString(),
        JsonValueKind.Number => cell.GetRawText(),
        JsonValueKind.True => "true",
        JsonValueKind.False => "false",
        _ => cell.GetRawText(),
    };

    private static ReportParameterDef ReadParameter(JsonElement item)
    {
        var name = Text(item, "name") ?? throw new FormatException("A report parameter has no name.");
        var type = Text(item, "type") switch
        {
            "text" => ReportParameterType.Text,
            "int" => ReportParameterType.IntegerNumber,
            "decimal" => ReportParameterType.DecimalNumber,
            "date" => ReportParameterType.Date,
            "link" => ReportParameterType.Link,
            "select" => ReportParameterType.Select,
            _ => ReportParameterType.Unknown,
        };
        JsonElement? fallback = item.TryGetProperty("default", out var def) && def.ValueKind != JsonValueKind.Null ? def.Clone() : null;
        ReportParameterOptions? options = null;
        if (item.TryGetProperty("options", out var o) && o.ValueKind == JsonValueKind.Object)
        {
            var values = new List<string>();
            if (o.TryGetProperty("values", out var v) && v.ValueKind == JsonValueKind.Array)
            {
                values.AddRange(v.EnumerateArray().Where(x => x.ValueKind == JsonValueKind.String).Select(x => x.GetString()!));
            }

            options = new ReportParameterOptions(values, Text(o, "doctype"));
        }

        return new ReportParameterDef(name, Text(item, "label") is { Length: > 0 } label ? label : name, type,
            item.TryGetProperty("required", out var r) && r.ValueKind == JsonValueKind.True, fallback, options);
    }

    private static List<ReportColumn> ReadColumns(JsonElement owner)
    {
        var columns = new List<ReportColumn>();
        if (owner.TryGetProperty("columns", out var list) && list.ValueKind == JsonValueKind.Array)
        {
            foreach (var item in list.EnumerateArray())
            {
                var name = Text(item, "name");
                if (string.IsNullOrEmpty(name))
                {
                    continue;
                }

                // `summable` is not in contract 0.7.0; a server that adds it is honoured, one that does not gets no totals.
                columns.Add(new ReportColumn(name, Text(item, "label") is { Length: > 0 } label ? label : name, Text(item, "type") ?? "text",
                    item.TryGetProperty("summable", out var s) && s.ValueKind == JsonValueKind.True));
            }
        }

        return columns;
    }

    private static string? Text(JsonElement owner, string property)
        => owner.ValueKind == JsonValueKind.Object && owner.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.String ? value.GetString() : null;
}
