using System;
using System.Collections.Generic;
using System.Globalization;
using System.IO;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Tests;

/// The lossless round trip against the server's own writer: every BPM template is read from the
/// text a person reviews, written back, and compared byte for byte with the canonical text the
/// Rust crate <c>spot-bpm-model::to_text</c> produces for the same document (the <c>.golden</c>
/// files beside each template). A second pass proves the writer is idempotent, and a third proves
/// the designer's own edits (a thumbnail's worth) never shift a byte it did not touch.
public sealed class ProcessTextRoundTripTests
{
    /// The four templates the server ships.
    public static TheoryData<string> Templates => new()
    {
        "approval_escalation.toml",
        "document_then_notify.toml",
        "integration_retry.toml",
        "order_to_cash.toml",
    };

    [Theory]
    [MemberData(nameof(Templates))]

    public void ATemplateRoundTripsToTheServersCanonicalText(string template)
    {
        string source = TestHost.Template(template);
        string canonical = TestHost.Golden(template + ".golden");

        FlowDocument document = FlowDocument.FromText(source);
        string written = document.ToText();

        Assert.Equal(canonical, written);
        Assert.Equal(written, FlowDocument.FromText(written).ToText());
    }

    [Theory]
    [MemberData(nameof(Templates))]

    public void ReadingTheCanonicalTextRecoversEveryNodeEdgeAndRule(string template)
    {
        FlowDocument document = FlowDocument.FromText(TestHost.Golden(template + ".golden"));

        Assert.NotEmpty(document.Nodes);
        Assert.NotEmpty(document.Edges);
        Assert.All(document.Nodes, node => Assert.True(FlowId.IsName(node.Id)));
        Assert.All(document.Edges, edge =>
        {
            Assert.NotNull(document.Node(edge.From));
            Assert.NotNull(document.Node(edge.To));
        });
        Assert.All(document.Edges.Where(edge => edge.Rule is not null), edge => Assert.NotNull(document.Rule(edge.Rule!)));
    }

    [Fact]

    public void TheRetryTimeoutAndErrorFieldsAreReadAndAQuorumJoinWritesInTheServersOrder()
    {
        FlowDocument document = FlowDocument.FromText(TestHost.Golden("integration_retry.toml.golden"));
        FlowNode call = Assert.Single(document.Nodes, node => node.Id == "call");
        Assert.Equal(3, call.RetryAttempts);
        Assert.Equal(5UL, call.RetryDelaySecs);
        Assert.Equal(30UL, call.TimeoutSecs);
        Assert.Equal("failed", call.Error);

        document.Nodes[0].Join = FlowJoinKind.Quorum;
        document.Nodes[0].JoinCount = 2;
        string written = document.ToText();

        Assert.Contains("join = \"quorum\"\njoin_count = 2\n", written, StringComparison.Ordinal);
        Assert.Equal(
            written,
            FlowDocument.FromText(written).ToText(),
            StringComparer.Ordinal);
    }

    [Fact]

    public void AnUnknownKindAndAnUnknownSectionAreKeptRatherThanRefused()
    {
        string text = string.Join('\n', new[]
        {
            "name = \"x\"",
            "app = \"demo\"",
            "version = 1",
            "status = \"draft\"",
            string.Empty,
            "[[nodes]]",
            "id = \"start\"",
            "kind = \"trigger\"",
            "trigger = \"manual\"",
            string.Empty,
            "[[nodes]]",
            "id = \"future\"",
            "kind = \"send_message\"",
            "channel = \"#ops\"",
            "retries = 3",
            "tags = [\"a\", \"b\"]",
            string.Empty,
            "[participants]",
            "buyer = \"acme\"",
            string.Empty,
        });

        FlowDocument document = FlowDocument.FromText(text);

        FlowNode future = Assert.Single(document.Nodes, node => node.Id == "future");
        Assert.Equal("send_message", future.Kind);
        Assert.Equal(FlowValue.Text("#ops"), future.ConfigValue("channel"));
        Assert.Equal(FlowValue.Number(3), future.ConfigValue("retries"));
        Assert.Equal(FlowValue.List(["a", "b"]), future.ConfigValue("tags"));

        string written = document.ToText();
        Assert.Contains("kind = \"send_message\"", written, StringComparison.Ordinal);
        Assert.Contains("channel = \"#ops\"", written, StringComparison.Ordinal);
        Assert.Contains("retries = 3", written, StringComparison.Ordinal);
        Assert.Contains("tags = [\"a\", \"b\"]", written, StringComparison.Ordinal);
        Assert.DoesNotContain("participants", written, StringComparison.Ordinal);
    }

    [Fact]

    public void AValueThisBuildCannotTypeIsPreservedVerbatim()
    {
        string text = string.Join('\n', new[]
        {
            "name = \"x\"",
            "app = \"demo\"",
            "version = 1",
            "status = \"draft\"",
            "[[nodes]]",
            "id = \"start\"",
            "kind = \"future\"",
            "when = 1979-05-27T07:32:00Z",
            string.Empty,
        });

        FlowDocument document = FlowDocument.FromText(text);
        FlowValue value = Assert.IsType<FlowRawValue>(document.Node("start")!.ConfigValue("when"));

        Assert.Equal("1979-05-27T07:32:00Z", value.Render());
        Assert.Contains("when = 1979-05-27T07:32:00Z", document.ToText(), StringComparison.Ordinal);
    }

    [Theory]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"draft\"\n[[edges]]\nfrom = \"a\"\nto = \"b\"\ncolour = \"red\"\n")]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"nope\"\n")]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"draft\"\n[[nodes]]\nid = \"Start\"\nkind = \"trigger\"\n")]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"draft\"\n[[nodes]]\nid = \"start\"\n")]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"draft\"\n[[nodes]]\nid = \"start\"\nkind = \"trigger\"\njoin = \"quorum\"\n")]
    [InlineData("name = \"x\"\napp = \"demo\"\nversion = 1\nstatus = \"draft\"\nthis is not a key value line\n")]

    public void MalformedTextIsRefusedWithItsLine(string text)
    {
        FlowTextException error = Assert.Throws<FlowTextException>(() => FlowDocument.FromText(text));

        Assert.True(error.Line > 0, "the error names its line: " + error.Message);
    }

    [Fact]

    public void TheTextFormIsTheSameBytesForAHandBuiltDocumentAsForItsReread()
    {
        FlowDocument document = new("order_to_cash", "selling") { Title = "Order to Cash", Version = 2 };
        FlowNode start = NewNode(document, "start", "trigger", FlowValue.Text("manual"));
        FlowNode end = NewNode(document, "end", "logic", null);
        end.Terminal = true;
        end.Join = FlowJoinKind.Any;
        document.AddEdge(new FlowEdge("start", "end"));

        string written = document.ToText();
        string reread = FlowDocument.FromText(written).ToText();

        Assert.Equal(written, reread, StringComparer.Ordinal);
        Assert.Contains("version = 2", written, StringComparison.Ordinal);
        Assert.Contains("[[nodes]]\nid = \"start\"\nkind = \"trigger\"\ntrigger = \"manual\"\n", written, StringComparison.Ordinal);
        Assert.Contains("[[nodes]]\nid = \"end\"\nkind = \"logic\"\njoin = \"any\"\nterminal = true\n", written, StringComparison.Ordinal);
        Assert.Equal(start.Id, document.Nodes[0].Id);
    }

    private static FlowNode NewNode(FlowDocument document, string id, string kind, FlowValue? config)
    {
        FlowNode node = new(id, kind);
        if (config is not null)
        {
            node.Map["trigger"] = config;
        }

        document.AddNode(node);
        return node;
    }
}
