using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The field mapping of a send node, written with the same expressions the rest of the platform
/// evaluates.
public sealed class MessageMappingTests
{

    private static ContractMessage Request() => ParticipantFixtures.Contract().Message("approval_request")!;

    [Fact]
    public void EveryFieldOfTheMessageGetsARowInTheContractsOrder()
    {
        MessageMapping mapping = new(Request());

        Assert.Equal(["amount_minor", "currency", "memo", "cost_code"], mapping.Rows.Select(row => row.Field.Name));
        Assert.All(mapping.Rows, row => Assert.False(row.IsMapped));
    }

    [Fact]
    public void AMappedFieldReportsWhatItReads()
    {
        MessageMapping mapping = new(Request());

        Assert.True(mapping.Set("amount_minor", "doc.total * 100", out string? error));

        Assert.Null(error);
        MessageMappingRow row = mapping.Rows[0];
        Assert.Equal(["doc.total"], row.Reads);
        Assert.Empty(row.Diagnostics);
        Assert.True(row.IsOk);
    }

    [Fact]
    public void AnExpressionThatDoesNotParseIsDiagnosedByTheExpressionsParser()
    {
        MessageMapping mapping = new(Request());
        mapping.Set("amount_minor", "doc.total *", out _);

        MessageMappingRow row = mapping.Rows[0];

        Assert.NotEmpty(row.Diagnostics);
        Assert.False(row.IsOk);
        Assert.Contains(mapping.Problems(), problem => problem.Contains("amount_minor", System.StringComparison.Ordinal));
    }

    [Fact]
    public void ARequiredFieldWithNoExpressionIsAProblemAndAnOptionalOneIsNot()
    {
        MessageMapping mapping = new(Request());
        mapping.Set("amount_minor", "doc.total", out _);

        System.Collections.Generic.IReadOnlyList<string> problems = mapping.Problems();

        Assert.Single(problems);
        Assert.Contains("currency", problems[0], System.StringComparison.Ordinal);
    }

    [Fact]
    public void ASecretFieldCannotBeMapped()
    {
        MessageMapping mapping = new(Request(), classifier: ParticipantFixtures.SecretCostCode());
        mapping.Set("cost_code", "doc.cost_code", out _);

        MessageMappingRow row = mapping.Rows.Single(r => r.Field.Name == "cost_code");

        Assert.True(row.IsSecret);
        Assert.False(row.IsOk);
        Assert.Contains(mapping.Problems(), problem => problem.Contains("secret", System.StringComparison.Ordinal));
    }

    [Fact]
    public void OnlyTheFieldsTheContractListsCanBeMapped()
    {
        MessageMapping mapping = new(Request());

        Assert.False(mapping.Set("password", "doc.password", out string? error));
        Assert.NotNull(error);
        Assert.False(mapping.Set("memo", "doc.a\ndoc.b", out _));
    }

    [Fact]
    public void TheMappingIsWrittenIntoTheNodeAndReadBack()
    {
        FlowDocument document = ParticipantFixtures.BuyerHalf();
        FlowEditor editor = new(document);
        MessageMapping mapping = new(Request());
        mapping.Set("amount_minor", "doc.total", out _);
        mapping.Set("currency", "upper(doc.currency)", out _);

        Assert.True(editor.Execute(mapping.ToCommand("send_message")!));

        string text = ((FlowStringValue)document.Node("send_message")!.ConfigValue("mapping")!).Value;
        Assert.Equal("amount_minor = doc.total\ncurrency = upper(doc.currency)", text);
        Assert.True(MessageMapping.IsFieldMap(text));
        MessageMapping again = new(Request(), text);
        Assert.Equal("upper(doc.currency)", again.Rows[1].Expression);
        FlowDocument reread = FlowDocument.FromText(document.ToText());
        Assert.Equal(text, ((FlowStringValue)reread.Node("send_message")!.ConfigValue("mapping")!).Value);
        Assert.True(editor.Undo());
        Assert.Null(document.Node("send_message")!.ConfigValue("mapping"));
    }

    [Theory]
    [InlineData("order_totals", false)]
    [InlineData("doc.a == doc.b", false)]
    [InlineData("amount_minor = doc.total", true)]
    [InlineData("amount_minor = doc.total\nbad line", false)]
    public void AValueWithoutFieldLinesIsAMappingNameAndStaysUntouched(string text, bool isFieldMap)
    {
        Assert.Equal(isFieldMap, MessageMapping.IsFieldMap(text));
    }
}
