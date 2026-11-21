using System.Text.Json.Nodes;

namespace Spotledger.Designer.Reports;

/// <summary>One edit of a report draft. The server applies it or refuses it in plain words.</summary>
public abstract record ReportOp
{
    /// <summary>The op's stable name (the serde tag).</summary>
    public abstract string Name { get; }

    /// <summary>The op as the server reads it.</summary>
    public JsonObject ToJson()
    {
        var node = new JsonObject { ["op"] = Name };
        Fill(node);
        return node;
    }

    /// <summary>Writes the op's own members.</summary>
    protected abstract void Fill(JsonObject node);

    /// <summary>Adds a column.</summary>
    public sealed record AddColumn(string Field, string? Label = null, Aggregate? Aggregate = null) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "add_column";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["field"] = Field;
            if (Label is not null)
            {
                node["label"] = Label;
            }

            if (Aggregate is { } aggregate)
            {
                node["aggregate"] = BuilderJson.Snake(aggregate);
            }
        }
    }

    /// <summary>Removes a column.</summary>
    public sealed record RemoveColumn(string Field) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "remove_column";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["field"] = Field;
    }

    /// <summary>Sets or clears a column's aggregate.</summary>
    public sealed record SetColumnAggregate(string Field, Aggregate? Aggregate) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_column_aggregate";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["field"] = Field;
            if (Aggregate is { } aggregate)
            {
                node["aggregate"] = BuilderJson.Snake(aggregate);
            }
        }
    }

    /// <summary>Declares a link.</summary>
    public sealed record AddJoin(string Alias, string DocType, string On) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "add_join";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["alias"] = Alias;
            node["doctype"] = DocType;
            node["on"] = On;
        }
    }

    /// <summary>Adds a value the viewer may supply.</summary>
    public sealed record AddParameter(DraftParameter Parameter) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "add_parameter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["parameter"] = BuilderJson.ToJson(Parameter);
    }

    /// <summary>Removes a value the viewer may supply.</summary>
    public sealed record RemoveParameter(string ParameterName) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "remove_parameter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["name"] = ParameterName;
    }

    /// <summary>Adds a condition every row must meet.</summary>
    public sealed record AddFilter(DraftFilter Filter) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "add_filter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["filter"] = BuilderJson.ToJson(Filter);
    }

    /// <summary>Removes the condition at an index.</summary>
    public sealed record RemoveFilter(int Index) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "remove_filter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["index"] = Index;
    }

    /// <summary>Groups the report by a field.</summary>
    public sealed record GroupBy(string Field) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "group_by";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["field"] = Field;
    }

    /// <summary>Stops grouping by a field.</summary>
    public sealed record UngroupBy(string Field) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "ungroup_by";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["field"] = Field;
    }

    /// <summary>Adds a sort key.</summary>
    public sealed record SortBy(string Field, SortDirection Direction) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "sort_by";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["field"] = Field;
            node["direction"] = BuilderJson.Snake(Direction);
        }
    }

    /// <summary>Removes every sort key.</summary>
    public sealed record ClearSort : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "clear_sort";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
        }
    }

    /// <summary>Sets the footer totals, by column name.</summary>
    public sealed record SetTotals(IReadOnlyList<string> Columns) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_totals";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["columns"] = BuilderJson.StringArray(Columns);
    }

    /// <summary>Sets or clears the chart.</summary>
    public sealed record SetChart(ChartSpec? Chart) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_chart";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            if (Chart is not null)
            {
                node["chart"] = BuilderJson.ToJson(Chart);
            }
        }
    }

    /// <summary>Sets the roles that may open the report (sharing).</summary>
    public sealed record SetRoles(IReadOnlyList<string> Roles) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_roles";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["roles"] = BuilderJson.StringArray(Roles);
    }

    /// <summary>Replaces the base record type.</summary>
    public sealed record SetBase(string DocType) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_base";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["doctype"] = DocType;
    }

    /// <summary>Sets the report's heading.</summary>
    public sealed record SetLabel(string Label) : ReportOp
    {
        /// <inheritdoc />
        public override string Name => "set_label";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["label"] = Label;
    }
}

/// <summary>Where a tile sits: a 1-based row and column and how many cells it spans.</summary>
public sealed record Place(int Row, int Column, int Width, int Height)
{
    /// <summary>The last row covered.</summary>
    public int LastRow => Row + Height - 1;

    /// <summary>The last column covered.</summary>
    public int LastColumn => Column + Width - 1;

    /// <summary>Whether two places share a cell.</summary>
    public bool Overlaps(Place other)
    {
        ArgumentNullException.ThrowIfNull(other);
        return Row <= other.LastRow && other.Row <= LastRow && Column <= other.LastColumn && other.Column <= LastColumn;
    }
}

/// <summary>What a tile shows.</summary>
public enum TileKind
{
    /// <summary>One large number.</summary>
    Number,

    /// <summary>A chart.</summary>
    Chart,

    /// <summary>The report as a table.</summary>
    Table,

    /// <summary>A short list of columns.</summary>
    List,
}

/// <summary>One tile of a dashboard draft; <see cref="Columns"/> is the number's column or the list's columns.</summary>
public sealed record Tile(string Id, string Title, TileKind Kind, string Report, Place Place, IReadOnlyList<string> Columns, ChartSpec? Chart = null, bool UsesFilters = true);

/// <summary>A filter shared by the dashboard's tiles.</summary>
public sealed record SharedFilter(string Name, string Label, ParamType Type, FilterValue? Default = null);

/// <summary>How often a dashboard refreshes.</summary>
public abstract record RefreshPolicy
{
    /// <summary>Only when the viewer asks.</summary>
    public sealed record Manual : RefreshPolicy;

    /// <summary>Each time the viewer opens it.</summary>
    public sealed record OnOpen : RefreshPolicy;

    /// <summary>On a timer, in whole seconds.</summary>
    public sealed record Every(int Seconds) : RefreshPolicy;
}

/// <summary>A dashboard draft as the server holds it.</summary>
public sealed record DashboardDraft(
    string Id,
    string Name,
    string Label,
    string Status,
    int GridColumns,
    int GridRows,
    RefreshPolicy Refresh,
    IReadOnlyList<SharedFilter> Filters,
    IReadOnlyList<Tile> Tiles,
    IReadOnlyList<string> Roles);

/// <summary>One edit of a dashboard draft.</summary>
public abstract record DashboardOp
{
    /// <summary>The op's stable name.</summary>
    public abstract string Name { get; }

    /// <summary>The op as the server reads it.</summary>
    public JsonObject ToJson()
    {
        var node = new JsonObject { ["op"] = Name };
        Fill(node);
        return node;
    }

    /// <summary>Writes the op's own members.</summary>
    protected abstract void Fill(JsonObject node);

    /// <summary>Adds a tile.</summary>
    public sealed record AddTile(Tile Tile) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "add_tile";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["tile"] = BuilderJson.ToJson(Tile);
    }

    /// <summary>Removes a tile.</summary>
    public sealed record RemoveTile(string Id) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "remove_tile";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["id"] = Id;
    }

    /// <summary>Moves or resizes a tile.</summary>
    public sealed record MoveTile(string Id, Place Place) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "move_tile";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["id"] = Id;
            node["place"] = BuilderJson.ToJson(Place);
        }
    }

    /// <summary>Changes a tile's title or whether it follows the shared filters.</summary>
    public sealed record SetTile(string Id, string Title, bool UsesFilters) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "set_tile";

        /// <inheritdoc />
        protected override void Fill(JsonObject node)
        {
            node["id"] = Id;
            node["title"] = Title;
            node["uses_filters"] = UsesFilters;
        }
    }

    /// <summary>Adds a filter shared by the tiles.</summary>
    public sealed record AddFilter(SharedFilter Filter) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "add_filter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["filter"] = BuilderJson.ToJson(Filter);
    }

    /// <summary>Removes a shared filter.</summary>
    public sealed record RemoveFilter(string FilterName) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "remove_filter";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["name"] = FilterName;
    }

    /// <summary>Sets the refresh policy.</summary>
    public sealed record SetRefresh(RefreshPolicy Refresh) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "set_refresh";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["refresh"] = BuilderJson.ToJson(Refresh);
    }

    /// <summary>Sets the roles that may open the dashboard (sharing).</summary>
    public sealed record SetRoles(IReadOnlyList<string> Roles) : DashboardOp
    {
        /// <inheritdoc />
        public override string Name => "set_roles";

        /// <inheritdoc />
        protected override void Fill(JsonObject node) => node["roles"] = BuilderJson.StringArray(Roles);
    }
}

/// <summary>What kind of person or rule approves.</summary>
public enum ApproverKind
{
    /// <summary>Everyone holding a role.</summary>
    Role,

    /// <summary>One named person.</summary>
    User,

    /// <summary>The person named in a field of the record.</summary>
    Field,

    /// <summary>The field's manager, then that manager's manager.</summary>
    ManagerChain,
}

/// <summary>Who is asked to approve.</summary>
public sealed record Approver(ApproverKind Kind, string Name);

/// <summary>One step that happens to the record when an outcome is reached.</summary>
public abstract record ApprovalAction
{
    /// <summary>Write a value into a field.</summary>
    public sealed record SetField(string Field, string Value) : ApprovalAction;

    /// <summary>Submit the record through its own lifecycle.</summary>
    public sealed record Submit : ApprovalAction;
}

/// <summary>When the approval starts.</summary>
public enum StartOn
{
    /// <summary>When a record of the type is created.</summary>
    Created,

    /// <summary>When a person starts it.</summary>
    Manual,
}

/// <summary>The approval workflow a business user describes: the server's template input.</summary>
public sealed record ApprovalTemplate(
    string Name,
    string Title,
    string App,
    string DocType,
    Approver Approvers,
    long SlaSeconds,
    Approver EscalateTo,
    IReadOnlyList<ApprovalAction> OnApprove,
    IReadOnlyList<ApprovalAction> OnReject,
    StartOn StartOn);
