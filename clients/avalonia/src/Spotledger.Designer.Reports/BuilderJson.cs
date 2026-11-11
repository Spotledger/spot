using System.Globalization;
using System.Text.Json.Nodes;

namespace Spotledger.Designer.Reports;

/// <summary>
/// The wire form of the builders' values, matching the serde shapes of spot-nocode-builders
/// (snake_case names, internally tagged unions). Hand-written so a renamed member cannot
/// silently change what is sent to the server.
/// </summary>
public static class BuilderJson
{
    /// <summary>The snake_case name of an enum member.</summary>
    public static string Snake(Enum value) => ToSnake(value.ToString());

    /// <summary>The wire name of a parameter type.</summary>
    public static string ParamName(ParamType type) => type switch
    {
        ParamType.Whole => "int",
        ParamType.Exact => "decimal",
        ParamType.Day => "date",
        _ => Snake(type),
    };

    /// <summary>Writes a filter literal.</summary>
    public static JsonObject ToJson(FilterValue value) => value switch
    {
        FilterValue.Text t => new JsonObject { ["kind"] = "text", ["text"] = t.Value },
        FilterValue.Whole i => new JsonObject { ["kind"] = "int", ["value"] = i.Value },
        FilterValue.Bool b => new JsonObject { ["kind"] = "bool", ["value"] = b.Value },
        FilterValue.Exact d => new JsonObject { ["kind"] = "decimal", ["text"] = d.Value },
        FilterValue.Day d => new JsonObject { ["kind"] = "date", ["text"] = d.Value },
        _ => throw new ArgumentOutOfRangeException(nameof(value)),
    };

    /// <summary>Writes a filter target.</summary>
    public static JsonObject ToJson(FilterTarget target) => target switch
    {
        FilterTarget.Parameter p => new JsonObject { ["kind"] = "parameter", ["name"] = p.Name },
        FilterTarget.Literal l => new JsonObject { ["kind"] = "value", ["value"] = ToJson(l.Value) },
        _ => throw new ArgumentOutOfRangeException(nameof(target)),
    };

    /// <summary>Writes a filter.</summary>
    public static JsonObject ToJson(DraftFilter filter) => new()
    {
        ["field"] = filter.Field,
        ["op"] = Snake(filter.Op),
        ["target"] = ToJson(filter.Target),
    };

    /// <summary>Writes a parameter.</summary>
    public static JsonObject ToJson(DraftParameter parameter)
    {
        var node = new JsonObject
        {
            ["name"] = parameter.Name,
            ["label"] = parameter.Label,
            ["ty"] = ParamName(parameter.Type),
            ["required"] = parameter.Required,
        };
        if (parameter.Default is { } value)
        {
            node["default"] = ToJson(value);
        }

        return node;
    }

    /// <summary>Writes a chart.</summary>
    public static JsonObject ToJson(ChartSpec chart)
    {
        var series = new JsonArray();
        foreach (var column in chart.Series)
        {
            series.Add(new JsonObject { ["column"] = column });
        }

        var node = new JsonObject { ["kind"] = Snake(chart.Kind) };
        if (chart.X is not null)
        {
            node["x"] = chart.X;
        }

        node["series"] = series;
        if (chart.Title is not null)
        {
            node["title"] = chart.Title;
        }

        return node;
    }

    /// <summary>Writes a tile place.</summary>
    public static JsonObject ToJson(Place place) => new()
    {
        ["row"] = place.Row,
        ["column"] = place.Column,
        ["width"] = place.Width,
        ["height"] = place.Height,
    };

    /// <summary>Writes a refresh policy.</summary>
    public static JsonNode ToJson(RefreshPolicy refresh) => refresh switch
    {
        RefreshPolicy.Manual => JsonValue.Create("manual")!,
        RefreshPolicy.OnOpen => JsonValue.Create("on_open")!,
        RefreshPolicy.Every every => new JsonObject { ["every"] = new JsonObject { ["seconds"] = every.Seconds } },
        _ => throw new ArgumentOutOfRangeException(nameof(refresh)),
    };

    /// <summary>Writes a tile.</summary>
    public static JsonObject ToJson(Tile tile)
    {
        var source = new JsonObject { ["kind"] = Snake(tile.Kind), ["report"] = tile.Report };
        switch (tile.Kind)
        {
            case TileKind.Number:
                source["column"] = tile.Columns.Count > 0 ? tile.Columns[0] : string.Empty;
                break;
            case TileKind.Chart:
                source["chart"] = tile.Chart is null ? null : ToJson(tile.Chart);
                break;
            case TileKind.List:
                source["columns"] = StringArray(tile.Columns);
                break;
        }

        return new JsonObject
        {
            ["id"] = tile.Id,
            ["title"] = tile.Title,
            ["source"] = source,
            ["place"] = ToJson(tile.Place),
            ["uses_filters"] = tile.UsesFilters,
        };
    }

    /// <summary>Writes a dashboard-wide filter.</summary>
    public static JsonObject ToJson(SharedFilter filter)
    {
        var node = new JsonObject { ["name"] = filter.Name, ["label"] = filter.Label, ["ty"] = ParamName(filter.Type) };
        if (filter.Default is { } value)
        {
            node["default"] = ToJson(value);
        }

        return node;
    }

    /// <summary>Writes an approver.</summary>
    public static JsonObject ToJson(Approver approver) => approver.Kind switch
    {
        ApproverKind.Role => new JsonObject { ["kind"] = "role", ["role"] = approver.Name },
        ApproverKind.User => new JsonObject { ["kind"] = "user", ["user"] = approver.Name },
        ApproverKind.Field => new JsonObject { ["kind"] = "field", ["field"] = approver.Name },
        ApproverKind.ManagerChain => new JsonObject { ["kind"] = "manager_chain", ["field"] = approver.Name },
        _ => throw new ArgumentOutOfRangeException(nameof(approver)),
    };

    /// <summary>Writes an outcome step.</summary>
    public static JsonObject ToJson(ApprovalAction action) => action switch
    {
        ApprovalAction.SetField s => new JsonObject { ["kind"] = "set_field", ["field"] = s.Field, ["value"] = s.Value },
        ApprovalAction.Submit => new JsonObject { ["kind"] = "submit" },
        _ => throw new ArgumentOutOfRangeException(nameof(action)),
    };

    /// <summary>Writes the approval template exactly as the server's ApprovalTemplate reads it.</summary>
    public static JsonObject ToJson(ApprovalTemplate template)
    {
        var approve = new JsonArray();
        foreach (var action in template.OnApprove)
        {
            approve.Add(ToJson(action));
        }

        var reject = new JsonArray();
        foreach (var action in template.OnReject)
        {
            reject.Add(ToJson(action));
        }

        return new JsonObject
        {
            ["name"] = template.Name,
            ["title"] = template.Title,
            ["app"] = template.App,
            ["doctype"] = template.DocType,
            ["approvers"] = ToJson(template.Approvers),
            ["sla_seconds"] = template.SlaSeconds,
            ["escalate_to"] = ToJson(template.EscalateTo),
            ["on_approve"] = approve,
            ["on_reject"] = reject,
            ["start_on"] = Snake(template.StartOn),
        };
    }

    /// <summary>Writes a list of strings.</summary>
    public static JsonArray StringArray(IEnumerable<string> values)
    {
        var array = new JsonArray();
        foreach (var value in values)
        {
            array.Add(value);
        }

        return array;
    }

    internal static string ToSnake(string text)
    {
        var builder = new System.Text.StringBuilder(text.Length + 4);
        for (var i = 0; i < text.Length; i++)
        {
            var c = text[i];
            if (char.IsUpper(c) && i > 0)
            {
                builder.Append('_');
            }

            builder.Append(char.ToLower(c, CultureInfo.InvariantCulture));
        }

        return builder.ToString();
    }
}
