using Spotledger.Client.TestSupport;
using Xunit;

namespace Spotledger.Client.TestSupport.Tests;

/// <summary>
/// WP-TST-14.T14: the first-frame wait and the settle rule are bounded by time (with the old 20
/// pumps as a floor), not by a count that a slow host exhausts before the first frame is drawn.
/// A fake frame source stands in for the window, so no rendering is involved.
/// </summary>
public sealed class ScreenshotAssertWaitTests
{
    private sealed class FakeFrame(byte[] pixels)
    {
        public byte[] Pixels { get; } = pixels;
    }

    [Fact]
    public void AFrameThatAppearsAfterMoreThanTwentyPumpsIsCaptured()
    {
        var pumps = 0;
        var frame = new FakeFrame(new byte[] { 1, 2, 3 });
        var captured = ScreenshotAssert.WaitForFrame(() => ++pumps > 45 ? frame : null, TimeSpan.FromSeconds(30));
        Assert.Same(frame, captured);
        Assert.Equal(46, pumps);
    }

    [Fact]
    public void NoFrameEverFailsWithTheElapsedTimeAndPumpCountAfterTheDeadline()
    {
        var pumps = 0;
        var failure = Assert.Throws<ScreenshotMismatchException>(
            () => ScreenshotAssert.WaitForFrame<FakeFrame>(() => { pumps++; return null; }, TimeSpan.FromMilliseconds(50)));
        Assert.Contains("No rendered frame after", failure.Message, StringComparison.Ordinal);
        Assert.Contains($"{pumps} pumps", failure.Message, StringComparison.Ordinal);
        Assert.Contains("ms", failure.Message, StringComparison.Ordinal);
        // The old budget is a floor: a tiny deadline never makes the wait stricter than 20 pumps.
        Assert.True(pumps >= 20, $"pumps {pumps}");
    }

    [Fact]
    public void AStableImageSettlesOnTheSecondIdenticalFrame()
    {
        var calls = 0;
        var settled = ScreenshotAssert.Settle(
            () => new FakeFrame(++calls < 3 ? new[] { (byte)calls } : new byte[] { 9, 9 }),
            frame => frame.Pixels,
            TimeSpan.FromMilliseconds(50));
        Assert.Equal(new byte[] { 9, 9 }, settled.Pixels);
        Assert.Equal(4, calls);
    }

    [Fact]
    public void AnUnstableImageStillRefuses()
    {
        var calls = 0;
        var failure = Assert.Throws<ScreenshotMismatchException>(() => ScreenshotAssert.Settle(
            () => new FakeFrame(new[] { (byte)(++calls % 250), (byte)(calls / 250) }),
            frame => frame.Pixels,
            TimeSpan.FromMilliseconds(50)));
        Assert.Contains("never settled", failure.Message, StringComparison.Ordinal);
        Assert.True(calls >= 20, $"attempts {calls}");
    }
}
