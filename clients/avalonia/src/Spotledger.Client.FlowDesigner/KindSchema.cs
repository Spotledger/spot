using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner;

/// The editor a kind's field asks for, chosen by the server's schema.
public enum FlowFieldType
{
    Text,
    MultilineText,
    WholeNumber,
    Boolean,
    Select,
    Expression,
    StringList,
}

/// One field of a node kind, as the server describes it.
public sealed record FlowKindField(
    string Key,
    string Label,
    FlowFieldType Type,
    bool Required = false,
    IReadOnlyList<string>? Options = null,
    string? Help = null,
    FlowValue? DefaultValue = null);

/// One node kind's config schema, as the server sends it.
public sealed record FlowKindSchema(
    string Kind,
    string DisplayName,
    string Category,
    IReadOnlyList<FlowKindField> Fields,
    IReadOnlyList<string>? ExpressionFields = null,
    bool TerminalByDefault = false)
{

    public FlowNode NewNode(string id)
    {
        FlowNode node = new(FlowId.Require(id, nameof(id)), Kind) { Terminal = TerminalByDefault };
        foreach (FlowKindField field in Fields)
        {
            if (field.DefaultValue is { } value)
            {
                node.Map[field.Key] = value;
            }
        }

        return node;
    }
}

/// The kinds the server offers: the palette, the property panels and the type-ahead all read this
/// one catalogue, so a kind the server adds needs no client release — it appears with its schema.
public sealed class FlowKindCatalogue
{

    private readonly Dictionary<string, FlowKindSchema> _byKind = new(StringComparer.Ordinal);

    public FlowKindCatalogue(IEnumerable<FlowKindSchema> schemas)
    {
        ArgumentNullException.ThrowIfNull(schemas);
        foreach (FlowKindSchema schema in schemas)
        {
            ArgumentNullException.ThrowIfNull(schema);

            // A field key becomes a key in the process text, so a key from the server that is not
            // a plain bare key is dropped here rather than written into the document.
            _byKind.TryAdd(schema.Kind, schema with
            {
                Fields = [.. schema.Fields.Where(field => IsBareKey(field.Key))],
            });
        }

        Kinds = [.. _byKind.Values
            .OrderBy(schema => schema.Category, StringComparer.Ordinal)
            .ThenBy(schema => schema.DisplayName, StringComparer.Ordinal)];
    }

    private static bool IsBareKey(string? key)
    {
        if (string.IsNullOrEmpty(key) || key.Length > 64)
        {
            return false;
        }

        foreach (char c in key)
        {
            bool ok = (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9') || c == '_' || c == '-';
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }

    public static FlowKindCatalogue Empty { get; } = new([]);

    public IReadOnlyList<FlowKindSchema> Kinds { get; }

    public FlowKindSchema? Schema(string kind)
    {
        ArgumentNullException.ThrowIfNull(kind);
        return _byKind.TryGetValue(kind, out FlowKindSchema? schema) ? schema : null;
    }

    public string DisplayName(string kind)
    {
        ArgumentNullException.ThrowIfNull(kind);
        return Schema(kind)?.DisplayName ?? kind;
    }

    public IReadOnlyList<FlowKindSchema> TypeAhead(string? typed)
    {
        string query = (typed ?? string.Empty).Trim();
        if (query.Length == 0)
        {
            return Kinds;
        }

        List<FlowKindSchema> starts = [];
        List<FlowKindSchema> contains = [];
        foreach (FlowKindSchema schema in Kinds)
        {
            if (Matches(schema, query, prefixOnly: true))
            {
                starts.Add(schema);
            }
            else if (Matches(schema, query, prefixOnly: false))
            {
                contains.Add(schema);
            }
        }

        starts.AddRange(contains);
        return starts;
    }

    private static bool Matches(FlowKindSchema schema, string query, bool prefixOnly)
    {
        const StringComparison Ordinal = StringComparison.OrdinalIgnoreCase;
        if (prefixOnly)
        {
            return schema.Kind.StartsWith(query, Ordinal) || schema.DisplayName.StartsWith(query, Ordinal);
        }

        return schema.Kind.Contains(query, Ordinal) || schema.DisplayName.Contains(query, Ordinal);
    }
}
