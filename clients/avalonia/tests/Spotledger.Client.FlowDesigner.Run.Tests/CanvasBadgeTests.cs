using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using Avalonia;
using Avalonia.Headless.XUnit;
using Avalonia.Media.Imaging;
using Spotledger.Client.FlowDesigner;
using Spotledger.Client.FlowDesigner.Run;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The badges are painted, not only computed: the canvas is rendered to a bitmap and the state
/// colour is found on the cards. This is the check a badge list cannot make for itself.
public sealed class CanvasBadgeTests
{

    private static readonly (byte R, byte G, byte B) Succeeded = (0x2F, 0x8A, 0x4B);

    private static readonly (byte R, byte G, byte B) Failed = (0xB4, 0x3A, 0x2C);

    [AvaloniaFact]

    public void ASucceededBadgeIsPaintedOnItsCard()
    {
        RunCanvas canvas = Canvas(
            RunFixtures.Snapshot(
                "run_1",
                RunOutcome.Running,
                ("webhook", RunNodeStatus.Succeeded),
                ("document", RunNodeStatus.Running),
                ("http", RunNodeStatus.Pending),
                ("mail", RunNodeStatus.Pending)),
            out _);

        byte[] pixels = Render(canvas, out int width, out int height, out int stride);

        Assert.True(Count(pixels, height, stride, Succeeded) > 40, "the succeeded badge is not painted on the card");
    }

    [AvaloniaFact]

    public void AFailedNodeOnTheErrorBranchIsOutlinedAndBadged()
    {
        RunSnapshot snapshot = new(
            "run_2",
            RunOutcome.Running,
            [
                new RunNodeSnapshot("webhook", RunNodeStatus.Succeeded),
                new RunNodeSnapshot("document", RunNodeStatus.Failed, ErrorBranch: true),
                new RunNodeSnapshot("http", RunNodeStatus.Running),
                new RunNodeSnapshot("mail", RunNodeStatus.Pending),
            ],
            CanStep: true,
            CanContinue: true);
        RunCanvas canvas = Canvas(snapshot, out _);

        byte[] pixels = Render(canvas, out int width, out int height, out int stride);

        Assert.Equal("Failed", canvas.BadgeText("document"));
        Assert.True(canvas.IsErrorBranch("document"));
        Assert.True(Count(pixels, height, stride, Failed) > 40, "the failed badge or the error outline is not painted");
    }

    private static RunCanvas Canvas(RunSnapshot snapshot, out FlowRunSession session)
    {
        RunCanvas canvas = new()
        {
            Catalogue = RunFixtures.Catalogue(),
            Document = RunFixtures.ScenarioDocument(),
        };
        FakeRunServer server = new() { StartResult = snapshot };
        session = new FlowRunSession(server, canvas.Document!, canvas.Catalogue);
        canvas.Session = session;
        session.SampleData.Text = RunFixtures.SampleJson;
        TestHost.Show(canvas, 900, 640);
        canvas.FitToContent();
        TestHost.Pump();
        TestHost.Await(session.StartTestAsync(TestContext.Current.CancellationToken));
        canvas.FitToContent();
        TestHost.Pump();
        return canvas;
    }

    private static byte[] Render(RunCanvas canvas, out int width, out int height, out int stride)
    {
        width = (int)Math.Max(1, canvas.Bounds.Width);
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

    /// Counts pixels close to a colour, so antialiasing does not decide the test. The bitmap is
    /// BGRA, which is why the channels are read back to front.
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
