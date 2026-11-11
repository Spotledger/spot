using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.FlowDesigner;

/// One diagnostic from an expression editor: a code, a sentence and a character offset.
public sealed record FlowDiagnostic(string Code, string Message, int Offset);

/// The <c>spot-expr</c> editing surface of the property panel: it reuses
/// <see cref="ExpressionParser"/> for diagnostics, so the editor and the evaluator agree, and
/// offers completion over the document fields the kind's schema declares plus the function
/// library. This is a UX hint only: the server still decides whether an expression is valid.
public static class FlowExpressionEditor
{

    public static IReadOnlyList<string> FunctionNames { get; } =
    [
        "abs", "coalesce", "contains", "date", "date_add", "datetime", "days_between", "decimal",
        "ends_with", "int", "is_empty", "len", "lower", "max", "min", "month", "now", "replace",
        "round", "starts_with", "substr", "text", "trim", "upper", "year", "day",
    ];

    public static IReadOnlyList<FlowDiagnostic> Diagnose(string? source)
    {
        if (string.IsNullOrWhiteSpace(source))
        {
            return [];
        }

        try
        {
            ExpressionParser.Parse(source);
            return [];
        }
        catch (ExpressionParseException error)
        {
            return [new FlowDiagnostic(error.Code, error.MessageText, error.Offset)];
        }
    }

    public static IReadOnlyList<string> Complete(
        string? source,
        int caret,
        IReadOnlyList<string>? fields,
        int limit = 12)
    {
        string text = source ?? string.Empty;
        int position = Math.Clamp(caret, 0, text.Length);
        int start = position;
        while (start > 0 && IsTokenCharacter(text[start - 1]))
        {
            start--;
        }

        string token = text[start..position];
        List<string> candidates = [];
        foreach (string field in fields ?? [])
        {
            if (FlowId.IsName(field))
            {
                candidates.Add("doc." + field);
            }
        }

        foreach (string function in FunctionNames)
        {
            candidates.Add(function + "(");
        }

        return [.. candidates
            .Where(candidate => token.Length == 0 || candidate.StartsWith(token, StringComparison.Ordinal))
            .Distinct(StringComparer.Ordinal)
            .Take(Math.Max(limit, 0))];
    }

    private static bool IsTokenCharacter(char c) =>
        (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c is '_' or '.';
}

/// One row of the property panel: its label, its current text and its diagnostics.
public sealed class FlowPropertyField
{

    private readonly IReadOnlyList<string> _expressionFields;

    internal FlowPropertyField(
        string key,
        string label,
        FlowFieldType type,
        bool required,
        IReadOnlyList<string>? options,
        string? help,
        FlowValue? value,
        IReadOnlyList<string> expressionFields)
    {
        Key = key;
        Label = label;
        Type = type;
        Required = required;
        Options = options ?? [];
        Help = help;
        Value = value;
        _expressionFields = expressionFields;
    }

    public string Key { get; }

    public string Label { get; }

    public FlowFieldType Type { get; }

    public bool Required { get; }

    public IReadOnlyList<string> Options { get; }

    public string? Help { get; }

    public FlowValue? Value { get; }

    public string Text => Value?.ToDisplayText() ?? string.Empty;

    public bool IsSet => Value is not null;

    public IReadOnlyList<FlowDiagnostic> Diagnostics
    {
        get
        {
            if (Required && !IsSet)
            {
                return [new FlowDiagnostic("flow.required", Label + " is required.", 0)];
            }

            return Type == FlowFieldType.Expression ? FlowExpressionEditor.Diagnose(Text) : [];
        }
    }

    public IReadOnlyList<string> Complete(int caret) =>
        Type == FlowFieldType.Expression
            ? FlowExpressionEditor.Complete(Text, caret, _expressionFields)
            : [];
}

/// The property panel for one node, generated from the server's kind schema: a field per config
/// key with the editor the schema asks for. A kind this build does not know still gets a panel —
/// its keys as plain text, kept exactly as written — because the server may know a kind this
/// client does not (ADR-080).
public sealed class FlowPropertyPanel
{

    private readonly FlowDocument _document;

    private readonly FlowKindSchema? _schema;

    private readonly List<FlowPropertyField> _fields = [];

    public FlowPropertyPanel(FlowDocument document, string nodeId, FlowKindCatalogue catalogue)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(nodeId);
        ArgumentNullException.ThrowIfNull(catalogue);
        _document = document;
        NodeId = nodeId;
        FlowNode? node = document.Node(nodeId) ?? throw new ArgumentException("There is no node " + nodeId + ".", nameof(nodeId));
        Kind = node.Kind;
        _schema = catalogue.Schema(node.Kind);
        IReadOnlyList<string> expressionFields = _schema?.ExpressionFields ?? [];

        if (_schema is null)
        {
            foreach (KeyValuePair<string, FlowValue> pair in node.Config.OrderBy(pair => pair.Key, StringComparer.Ordinal))
            {
                _fields.Add(new FlowPropertyField(
                    pair.Key,
                    pair.Key,
                    FlowFieldType.Text,
                    required: false,
                    options: null,
                    help: null,
                    pair.Value,
                    expressionFields));
            }

            return;
        }

        foreach (FlowKindField field in _schema.Fields)
        {
            _fields.Add(new FlowPropertyField(
                field.Key,
                field.Label,
                field.Type,
                field.Required,
                field.Options,
                field.Help,
                node.ConfigValue(field.Key),
                expressionFields));
        }
    }

    public string NodeId { get; }

    public string Kind { get; }

    public string KindName => _schema?.DisplayName ?? Kind;

    public string Heading => KindName + " · " + NodeId;

    public bool IsUnknownKind => _schema is null;

    public string? UnknownKindNotice => _schema is null ? FlowStrings.UnknownKind(Kind) : null;

    public IReadOnlyList<FlowPropertyField> Fields => _fields;

    public bool HasDiagnostics => _fields.Any(row => row.Diagnostics.Count > 0);

    public FlowPropertyField? Field(string key)
    {
        ArgumentNullException.ThrowIfNull(key);
        return _fields.FirstOrDefault(field => string.Equals(field.Key, key, StringComparison.Ordinal));
    }

    public bool TrySet(string key, string text, out IFlowCommand? command, out string? error)
    {
        ArgumentNullException.ThrowIfNull(key);
        command = null;
        error = null;
        FlowPropertyField row = Field(key) ?? throw new ArgumentException("No field " + key + ".", nameof(key));
        FlowNode? node = _document.Node(NodeId);
        if (node is null)
        {
            error = "The node is gone.";
            return false;
        }

        string value = text.Trim();
        if (row.Type == FlowFieldType.Expression)
        {
            IReadOnlyList<FlowDiagnostic> diagnostics = FlowExpressionEditor.Diagnose(value);
            if (diagnostics.Count > 0)
            {
                error = diagnostics[0].Message;
                return false;
            }
        }

        if (value.Length == 0)
        {
            if (row.Required)
            {
                error = row.Label + " is required.";
                return false;
            }

            command = new SetConfigCommand(NodeId, key, null);
            return true;
        }

        FlowValue? parsed = row.Type switch
        {
            FlowFieldType.Text or FlowFieldType.MultilineText => FlowValue.Text(text.Trim()),
            FlowFieldType.Expression => FlowValue.Text(text.Trim()),
            FlowFieldType.Boolean => ParseFlag(value, row, out error),
            FlowFieldType.WholeNumber => ParseInteger(value, row, out error),
            FlowFieldType.Select => ParseOption(value, row, out error),
            FlowFieldType.StringList => ParseList(text),
            _ => null,
        };

        if (parsed is null)
        {
            return false;
        }

        command = new SetConfigCommand(NodeId, key, parsed);
        return true;
    }

    private static FlowValue? ParseFlag(string text, FlowPropertyField field, out string? error)
    {
        error = null;
        if (bool.TryParse(text, out bool flag))
        {
            return FlowValue.Flag(flag);
        }

        error = field.Label + " must be true or false.";
        return null;
    }

    private static FlowValue? ParseInteger(string text, FlowPropertyField field, out string? error)
    {
        error = null;
        if (long.TryParse(text, NumberStyles.AllowLeadingSign, CultureInfo.InvariantCulture, out long number))
        {
            return FlowValue.Number(number);
        }

        error = field.Label + " must be a whole number.";
        return null;
    }

    private static FlowValue? ParseOption(string text, FlowPropertyField field, out string? error)
    {
        error = null;
        if (field.Options.Contains(text, StringComparer.Ordinal))
        {
            return FlowValue.Text(text);
        }

        error = field.Label + " must be one of: " + string.Join(", ", field.Options) + ".";
        return null;
    }

    private static FlowValue ParseList(string text)
    {
        List<string> items = [];
        foreach (string line in text.Replace("\r\n", "\n", StringComparison.Ordinal).Split('\n'))
        {
            string item = line.Trim();
            if (item.Length > 0)
            {
                items.Add(item);
            }
        }

        return FlowValue.List(items);
    }
}
