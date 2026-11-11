using System;
using System.Collections.Generic;
using System.Linq;
using System.Runtime.InteropServices;
using System.Threading.Tasks;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.Media.Imaging;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The participant view is painted, not only computed: the canvas is rendered to a bitmap and the
/// colours of the lanes, the message lines, the effect badges, the commit regions and the Undo edges
/// are found on it. This is the check a list of rows cannot make for itself.
public sealed class CanvasTests
{

    private static readonly (byte R, byte G, byte B) OwnLane = (0x25, 0x63, 0xEB);

    private static readonly (byte R, byte G, byte B) OtherLane = (0x6B, 0x72, 0x80);

    private static readonly (byte R, byte G, byte B) Message = (0x7C, 0x3A, 0xED);

    private static readonly (byte R, byte G, byte B) Region = (0xF5, 0x9E, 0x0B);

    private static readonly (byte R, byte G, byte B) Compensable = (0xB7, 0x79, 0x1F);

    private static readonly (byte R, byte G, byte B) Undo = (0xDB, 0x27, 0x77);

    private static readonly (byte R, byte G, byte B) Rewind = (0x0E, 0x74, 0x90);

    private const string UndoFlow =
        """
        name = "undo_flow"
        app = "procurement"
        version = 1
        status = "draft"

        [[nodes]]
        id = "send_message"
        kind = "send_message"
        contract = "purchase_approval"
        message = "approval_request"

        [[nodes]]
        id = "wait_message"
        kind = "wait_message"
        contract = "purchase_approval"
        message = "approval_decision"

        [[nodes]]
        id = "fix"
        kind = "form"

        [[edges]]
        from = "send_message"
        to = "wait_message"

        [[edges]]
        from = "wait_message"
        to = "fix"

        [[edges]]
        from = "fix"
        to = "send_message"
        """;

    [AvaloniaFact]
    public void TheTwoOrganisationRequestApproveFlowIsBuiltWithTheKeyboardAndFollowsTheContract()
    {
        FakeParticipantServer server = new();
        FlowDocument document = new("buy_approval", "procurement");
        ParticipantSession session = new(
            server,
            new FakeSimulatorServer(),
            document,
            new FlowEditor(document),
            new ContractEditor(ParticipantFixtures.Contract()),
            ParticipantFixtures.Buyer,
            ParticipantFixtures.ServerKinds());
        ParticipantCanvas canvas = new() { Document = document, Editor = session.Editor, Catalogue = session.Catalogue, Session = session };
        TestHost.Show(canvas, 1024, 700);

        AddFromThePalette(canvas, "form");
        AddFromThePalette(canvas, "send");
        AddFromThePalette(canvas, "wait");
        AddFromThePalette(canvas, "commit");
        AddFromThePalette(canvas, "form");

        Assert.Equal(["form", "send_message", "wait_message", "commit", "form_2"], document.Nodes.Select(node => node.Id));
        Assert.Equal(["form", "send_message", "wait_message", "commit", "form"], document.Nodes.Select(node => node.Kind));
        Assert.Equal("Wait for message", canvas.Catalogue.DisplayName("wait_message"));
        Assert.Equal("Commit point", canvas.Catalogue.DisplayName("commit"));

        ConnectWithTheKeyboard(canvas, "form", "send_message");
        ConnectWithTheKeyboard(canvas, "send_message", "wait_message");
        ConnectWithTheKeyboard(canvas, "wait_message", "commit");
        ConnectWithTheKeyboard(canvas, "wait_message", "form_2");

        // The property panels are the schemas': the choices are the contract's own messages.
        Configure(session, "send_message", "message", "approval_request");
        Configure(session, "wait_message", "message", "approval_decision");
        Configure(session, "wait_message", "on_timeout", "timeout_edge");
        Configure(session, "form", "assignee", "role:buyer");
        Configure(session, "form", "fields", "amount_minor\ncurrency");
        Configure(session, "form_2", "assignee", "role:buyer");
        Configure(session, "form_2", "fields", "reason");
        Assert.True(session.Editor.Execute(new SetEdgeFieldCommand(new FlowEdgeKey("wait_message", "form_2"), "label", ParticipantKinds.TimeoutEdgeLabel)));
        FlowNode wait = document.Node("wait_message")!;
        Assert.True(session.Editor.Execute(new SetNodeSettingsCommand("wait_message", FlowNodeSettings.Of(wait) with { TimeoutSecs = 86_400 })));

        // The field mapping, with the platform's expressions.
        MessageMapping mapping = new(session.Contract.Current.Message("approval_request")!);
        Assert.True(mapping.Set("amount_minor", "doc.total", out _));
        Assert.True(mapping.Set("currency", "upper(doc.currency)", out _));
        Assert.Empty(mapping.Problems());
        Assert.True(session.Editor.Execute(mapping.ToCommand("send_message")!));

        Assert.True(canvas.ApplyAutoLayout());
        canvas.FitToLanes();
        TestHost.Pump();

        // What the server would store: a round trip, and the shape the conformance check reads.
        string saved = document.ToText();
        FlowDocument reread = FlowDocument.FromText(saved);
        Assert.Equal(saved, reread.ToText());
        Assert.Equal(
            [new FlowEdgeKey("form", "send_message"), new FlowEdgeKey("send_message", "wait_message"), new FlowEdgeKey("wait_message", "commit"), new FlowEdgeKey("wait_message", "form_2")],
            reread.Edges.Select(edge => edge.Key));
        Assert.Contains("kind = \"send_message\"", saved, StringComparison.Ordinal);
        Assert.Contains("message = \"approval_request\"", saved, StringComparison.Ordinal);
        Assert.Contains("on_timeout = \"timeout_edge\"", saved, StringComparison.Ordinal);
        Assert.Contains("timeout_secs = 86400", saved, StringComparison.Ordinal);
        Assert.Contains("label = \"timeout\"", saved, StringComparison.Ordinal);

        // The lanes: the own lane highlighted, the approver hidden, messages the only way across.
        Assert.Equal(2, canvas.Bands.Count);
        Assert.True(canvas.Bands[0].IsOwn);
        Assert.Equal("approver", canvas.Bands[1].Role);
        Assert.Equal(
            [("send_message", MessageDirection.Out), ("wait_message", MessageDirection.In)],
            canvas.MessageLinks.Select(link => (link.NodeId, link.Direction)));
        Assert.All(canvas.MessageLinks, link => Assert.True(link.Known));
        Assert.Equal(["form", "send_message", "wait_message"], canvas.HeldNodes.OrderBy(id => document.Nodes.Select(node => node.Id).ToList().IndexOf(id)));

        // The server finds this half conforming; the same texts it was asked with are the saved ones.
        FlowActionResult result = TestHost.Await(session.CheckConformanceAsync(TestContext.Current.CancellationToken));
        Assert.True(result.Ok);
        Assert.Equal(saved, server.Conformed[0].Process);
    }

    [AvaloniaFact]
    public void TheLanesAndTheMessageLinesArePaintedAroundTheSteps()
    {
        ParticipantCanvas canvas = Canvas(ParticipantFixtures.BuyerHalf());

        byte[] pixels = Render(canvas, out int height, out int stride);

        Assert.True(Count(pixels, height, stride, OwnLane) > 200, "the own lane is not highlighted");
        Assert.True(Count(pixels, height, stride, OtherLane) > 100, "the other lane is not drawn");
        Assert.True(Count(pixels, height, stride, Message) > 40, "the message lines are not drawn");
    }

    [AvaloniaFact]
    public void WithoutLanesNothingOfThemIsPainted()
    {
        ParticipantCanvas canvas = Canvas(ParticipantFixtures.BuyerHalf());
        canvas.Lanes = null;
        canvas.InvalidateVisual();
        TestHost.Pump();

        byte[] pixels = Render(canvas, out int height, out int stride);

        Assert.Equal(0, Count(pixels, height, stride, Message));
        Assert.Empty(canvas.Bands);
        Assert.Empty(canvas.MessageLinks);
    }

    [AvaloniaFact]
    public void CommitPointRegionsAreShadedAndCanBeSwitchedOff()
    {
        ParticipantCanvas canvas = Canvas(ParticipantFixtures.BuyerHalf());

        int shaded = Count(Render(canvas, out int height, out int stride), height, stride, Region);
        canvas.ShowCommitRegions = false;
        canvas.InvalidateVisual();
        TestHost.Pump();
        int plain = Count(Render(canvas, out height, out stride), height, stride, Region);

        Assert.True(shaded > 60, "the held region is not shaded");
        Assert.True(plain < shaded / 4, "switching the shading off still paints it");
    }

    [AvaloniaFact]
    public void EffectBadgesAreOnTheStepsTheServerSaysDeclareEffects()
    {
        ParticipantCanvas canvas = Canvas(ParticipantFixtures.BuyerHalf());
        int before = Count(Render(canvas, out int height, out int stride), height, stride, Compensable);

        canvas.Effects = new EffectBadges(ParticipantFixtures.EffectSummary());
        canvas.InvalidateVisual();
        TestHost.Pump();
        int after = Count(Render(canvas, out height, out stride), height, stride, Compensable);

        Assert.Equal("Compensable, internal", canvas.Effects.Badge("commit")!.Text);
        Assert.True(after > before + 60, "the compensable badge is not painted on the commit point");
    }

    [AvaloniaFact]
    public async Task AnUndoEdgeIsTypedAndTheServersErrorIsShownOnTheEdge()
    {
        ParticipantCanvas canvas = Canvas(FlowDocument.FromText(UndoFlow));
        FlowEdgeKey back = new("fix", "send_message");

        // Untyped: a hint, no pill.
        Assert.Equal(EdgeMode.Forward, EdgeModes.Of(canvas.Document!.Edges.Single(edge => edge.Key == back)));
        Assert.Contains(canvas.AllProblems, problem => problem.Code == "back_edge_untyped" && problem.Edge == back);
        int untyped = Count(Render(canvas, out int height, out int stride), height, stride, Undo);

        // Typed Undo through the editor, as the person would.
        Assert.True(canvas.Editor!.Execute(EdgeModes.SetMode(canvas.Document, back, EdgeMode.Undo)!));
        Assert.DoesNotContain(canvas.AllProblems, problem => problem.Code == "back_edge_untyped");
        int typed = Count(Render(canvas, out height, out stride), height, stride, Undo);
        Assert.True(typed > untyped + 60, "the Undo mark is not painted on the edge");

        // The server says the undo passes an irreversible effect: the code is on the edge.
        FakeDesignerServer designer = new()
        {
            Problems = [new FlowProblem("undo_past_irreversible", "the undo edge passes an irreversible effect", FlowProblemSeverity.Error, EdgeFrom: "fix", EdgeTo: "send_message")],
        };
        canvas.Validation = new FlowValidationCoordinator(designer, canvas.Editor, TimeSpan.FromSeconds(1));
        await canvas.Validation.ValidateNowAsync(TestContext.Current.CancellationToken);
        canvas.InvalidateVisual();
        TestHost.Pump();

        FlowProblem onEdge = Assert.Single(canvas.ProblemsOnEdge(back));
        Assert.Equal("undo_past_irreversible", onEdge.Code);
        Assert.Contains(canvas.ProblemRows, row => row.Problem.Code == "undo_past_irreversible");
        int withError = Count(Render(canvas, out height, out stride), height, stride, Undo);
        Assert.True(withError > typed + 40, "the server's error is not painted on the edge");
    }

    [AvaloniaFact]
    public void ARewindEdgeIsMarkedInItsOwnColour()
    {
        ParticipantCanvas canvas = Canvas(FlowDocument.FromText(UndoFlow));
        Assert.True(canvas.Editor!.Execute(EdgeModes.SetMode(canvas.Document!, new FlowEdgeKey("fix", "send_message"), EdgeMode.Rewind)!));

        int rewind = Count(Render(canvas, out int height, out int stride), height, stride, Rewind);

        Assert.True(rewind > 60, "the Rewind mark is not painted on the edge");
        Assert.Equal("Rewind", EdgeMode.Rewind.Word());
    }

    [AvaloniaFact]
    public void ConformanceProblemsAreMarkedOnTheStepsAndListedWithTheServersOwn()
    {
        FakeParticipantServer server = new()
        {
            ConformanceProblems = [new ContractProblem("bpm_contract.conformance_timeout", "wait_message", "wait node `wait_message` has no timeout branch")],
        };
        FlowDocument document = ParticipantFixtures.BuyerHalf();
        ParticipantSession session = new(
            server,
            new FakeSimulatorServer(),
            document,
            new FlowEditor(document),
            new ContractEditor(ParticipantFixtures.Contract()),
            ParticipantFixtures.Buyer,
            ParticipantFixtures.ServerKinds());
        ParticipantCanvas canvas = new() { Document = document, Editor = session.Editor, Catalogue = session.Catalogue, Session = session };
        TestHost.Show(canvas, 1024, 700);
        canvas.ApplyAutoLayout();
        canvas.FitToLanes();
        TestHost.Pump();
        int before = Count(Render(canvas, out int height, out int stride), height, stride, Undo);

        TestHost.Await(session.CheckConformanceAsync(TestContext.Current.CancellationToken));
        TestHost.Pump();

        FlowProblem problem = Assert.Single(canvas.ProblemsOnNode("wait_message"));
        Assert.Equal("bpm_contract.conformance_timeout", problem.Code);
        Assert.Contains(canvas.AllProblems, p => p.Code == "bpm_contract.conformance_timeout");
        Assert.True(Count(Render(canvas, out height, out stride), height, stride, Undo) > before + 8, "the step carries no problem mark");
    }

    private static ParticipantCanvas Canvas(FlowDocument document)
    {
        FakeParticipantServer server = new() { Effects = [] };
        ParticipantSession session = new(
            server,
            new FakeSimulatorServer(),
            document,
            new FlowEditor(document),
            new ContractEditor(ParticipantFixtures.Contract()),
            ParticipantFixtures.Buyer,
            ParticipantFixtures.ServerKinds());
        ParticipantCanvas canvas = new() { Document = document, Editor = session.Editor, Catalogue = session.Catalogue, Session = session };
        TestHost.Show(canvas, 1024, 700);
        canvas.ApplyAutoLayout();
        canvas.FitToLanes();
        TestHost.Pump();
        return canvas;
    }

    private static void Configure(ParticipantSession session, string node, string key, string text)
    {
        FlowPropertyPanel panel = new(session.Document, node, session.Catalogue);
        Assert.True(panel.TrySet(key, text, out IFlowCommand? command, out string? error), error);
        Assert.True(session.Editor.Execute(command!));
    }

    private static void AddFromThePalette(ParticipantCanvas canvas, string query)
    {
        Assert.True(canvas.HandleKey(Key.A, KeyModifiers.None));
        canvas.TypeTypeAhead(query);
        Assert.True(canvas.CommitTypeAhead());
        TestHost.Pump();
    }

    private static void ConnectWithTheKeyboard(ParticipantCanvas canvas, string from, string to)
    {
        // Escape drops the selection, and the next arrow key walks to the following step: repeat
        // until the walk reaches the source (an arrow key on a selected step would nudge it).
        canvas.HandleKey(Key.Escape, KeyModifiers.None);
        for (int guard = 0; guard < 16; guard++)
        {
            Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
            if (string.Equals(canvas.Selection.SingleNodeId, from, StringComparison.Ordinal))
            {
                break;
            }

            canvas.HandleKey(Key.Escape, KeyModifiers.None);
        }

        Assert.Equal(from, canvas.Selection.SingleNodeId);
        Assert.True(canvas.HandleKey(Key.C, KeyModifiers.None));
        for (int guard = 0; guard < 16 && !string.Equals(canvas.ConnectCandidate, to, StringComparison.Ordinal); guard++)
        {
            Assert.True(canvas.HandleKey(Key.Right, KeyModifiers.None));
        }

        Assert.Equal(to, canvas.ConnectCandidate);
        Assert.True(canvas.HandleKey(Key.Enter, KeyModifiers.None));
        Assert.Null(canvas.ConnectFrom);
        TestHost.Pump();
    }

    private static byte[] Render(ParticipantCanvas canvas, out int height, out int stride)
    {
        int width = (int)Math.Max(1, canvas.Bounds.Width);
        height = (int)Math.Max(1, canvas.Bounds.Height);
        stride = width * 4;
        byte[] pixels = new byte[stride * height];
        using RenderTargetBitmap target = new(new PixelSize(width, height));
        target.Render(canvas);
        IntPtr buffer = Marshal.AllocHGlobal(pixels.Length);
        try
        {
            target.CopyPixels(new PixelRect(0, 0, width, height), buffer, pixels.Length, stride);
            Marshal.Copy(buffer, pixels, 0, pixels.Length);
        }
        finally
        {
            Marshal.FreeHGlobal(buffer);
        }

        return pixels;
    }

    /// Counts pixels close to a colour, so antialiasing does not decide the test. The bitmap is BGRA.
    private static int Count(byte[] pixels, int height, int stride, (byte R, byte G, byte B) colour)
    {
        int found = 0;
        for (int y = 0; y < height; y++)
        {
            for (int x = 0; x < stride / 4; x++)
            {
                int offset = (y * stride) + (x * 4);
                if (Near(pixels[offset], colour.B) && Near(pixels[offset + 1], colour.G) && Near(pixels[offset + 2], colour.R))
                {
                    found++;
                }
            }
        }

        return found;
    }

    private static bool Near(byte actual, byte expected) => Math.Abs(actual - expected) <= 12;
}
