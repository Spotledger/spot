using System.Text.Json;
using System.Text.Json.Nodes;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Lists.Tests;

/// <summary>The filter builder produces exactly the contract's filter JSON for every operator and field
/// kind (golden file), refuses what the field cannot take, and stays inside the server's limits.</summary>
public sealed class FilterBuilderTests
{
    private sealed record Case(string Name, string Field, string Op, string[] Values, JsonElement? Expected);

    private static readonly JsonSerializerOptions CaseOptions = new() { PropertyNameCaseInsensitive = true };

    private static Case[] Load(string file)
        => JsonSerializer.Deserialize<Case[]>(File.ReadAllText(Path.Combine(AppContext.BaseDirectory, "Golden", file)), CaseOptions)!;

    public static IEnumerable<object[]> Golden() => Load("filters.json").Select(c => new object[] { c.Name });
    public static IEnumerable<object[]> Invalid() => Load("filters-invalid.json").Select(c => new object[] { c.Name });

    [Theory]
    [MemberData(nameof(Golden))]
    public void ProducesExactlyTheContractsJson(string name)
    {
        var c = Load("filters.json").Single(x => x.Name == name);
        var model = Fixtures.Model();
        var condition = new FilterCondition(c.Field, Enum.Parse<FilterOperator>(c.Op), c.Values);

        Assert.True(condition.TryBuild(model.Field(c.Field)!, out var filter, out var error), error);

        var actual = JsonNode.Parse(ListQuery.FilterJson(filter!))!;
        var expected = JsonNode.Parse(c.Expected!.Value.GetRawText())!;
        Assert.True(JsonNode.DeepEquals(expected, actual), $"{name}: expected {expected.ToJsonString()} but built {actual.ToJsonString()}");
    }

    [Theory]
    [MemberData(nameof(Invalid))]
    public void RefusesWhatTheFieldCannotTake(string name)
    {
        var c = Load("filters-invalid.json").Single(x => x.Name == name);
        var model = Fixtures.Model();
        var field = model.Field(c.Field) ?? new FieldModel("title", "Title", FieldKind.Text);
        var condition = new FilterCondition(c.Field, Enum.Parse<FilterOperator>(c.Op), c.Values);

        Assert.False(condition.TryBuild(field, out var filter, out var error));
        Assert.Null(filter);
        Assert.False(string.IsNullOrEmpty(error));
    }

    [Fact]
    public void EveryOperatorOfEveryKindIsCoveredByAGoldenCase()
    {
        var covered = Load("filters.json").Select(c => (Fixtures.Model().Field(c.Field)!.Kind, c.Op)).ToHashSet();
        foreach (var field in Fixtures.Model().Fields.Where(f => FilterCondition.IsFilterable(f.Kind)))
        {
            foreach (var op in FilterCondition.OperatorsFor(field.Kind))
            {
                Assert.Contains((field.Kind, op.ToString()), covered);
            }
        }
    }

    [Fact]
    public void RequestJsonCarriesFieldsOrderLimitCursorAndTotalAsData()
    {
        var query = new ListQuery(["title", "amount"], new CompareFilter("amount", CompareOp.Gt, ListScalar.Of("1000.00")), [new ListOrder("title", true)]);

        var json = JsonNode.Parse(query.ToRequestJson(50, "c:50", true))!;

        Assert.Equal("""{"fields":["title","amount"],"filter":{"field":"amount","op":"gt","value":"1000.00"},"order":[{"field":"title","dir":"desc"}],"limit":50,"cursor":"c:50","with_total":true}""", json.ToJsonString());
    }

    [Fact]
    public void ARequestNeverCarriesTotalOrCursorUnlessAsked()
    {
        var json = JsonNode.Parse(new ListQuery(["title"]).ToRequestJson(20, null, false))!.AsObject();

        Assert.False(json.ContainsKey("with_total"));
        Assert.False(json.ContainsKey("cursor"));
        Assert.False(json.ContainsKey("filter"));
    }

    [Fact]
    public void FieldNamesThatAreNotIdentifiersNeverReachTheWire()
    {
        Assert.Throws<ArgumentException>(() => new ListQuery(["title\"}, {\"x"]).ToRequestJson(20, null, false));
        Assert.Throws<ArgumentException>(() => new ListQuery(["title"], new NullFilter("Bad Name", true)).ToRequestJson(20, null, false));
        Assert.Throws<ArgumentOutOfRangeException>(() => new ListQuery(["title"]).ToRequestJson(501, null, false));
    }

    [Fact]
    public void FiltersLargerThanTheServerAcceptsAreRefusedBeforeTheyAreSent()
    {
        ListFilter deep = new NullFilter("title", true);
        for (var i = 0; i < ListLimits.MaxDepth; i++)
        {
            deep = new NotFilter(deep);
        }

        var wide = new AndFilter(Enumerable.Range(0, ListLimits.MaxNodes).Select(_ => (ListFilter)new NullFilter("title", true)).ToArray());

        Assert.False(ListLimits.Fits(deep));
        Assert.False(ListLimits.Fits(wide));
        Assert.False(ListLimits.Fits(new InFilter("title", Enumerable.Range(0, 1001).Select(i => ListScalar.Of(i.ToString(System.Globalization.CultureInfo.InvariantCulture))).ToArray())));
        Assert.True(ListLimits.Fits(new AndFilter([new NullFilter("title", true), new NotFilter(new NullFilter("due", true))])));
        Assert.Throws<InvalidOperationException>(() => new ListQuery(["title"], wide).ToRequestJson(20, null, false));
    }

    [Fact]
    public void AllConditionsAreAndedAndAnUnknownFieldIsRefused()
    {
        var model = Fixtures.Model();
        var ok = FilterCondition.TryBuildAll(model,
            [new FilterCondition("title", FilterOperator.Contains, ["a"]), new FilterCondition("qty", FilterOperator.Equals, ["3"])],
            out var filter, out _);
        var unknown = FilterCondition.TryBuildAll(model, [new FilterCondition("nope", FilterOperator.Contains, ["a"])], out _, out var error);

        Assert.True(ok);
        Assert.IsType<AndFilter>(filter);
        Assert.False(unknown);
        Assert.NotNull(error);
    }
}
