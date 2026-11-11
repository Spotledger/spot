using System.Text;
using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>Redaction: a token never reaches a log line, an exception message or ToString.</summary>
public sealed class RedactionTests
{
    private const string Token = "3q2-7wEBAgMEBQYHCAkKCwwNDg8QERITFBUWFxgZGhs";

    [Fact]
    public void The_auth_message_ToString_redacts_the_token()
    {
        var message = new RealtimeAuthMessage(Token);

        Assert.DoesNotContain(Token, message.ToString(), StringComparison.Ordinal);
        Assert.Contains("***", message.ToString(), StringComparison.Ordinal);

        // The token still goes on the wire: redaction is about rendering, not transport.
        Assert.Contains(Token, Encoding.UTF8.GetString(RealtimeMessageCodec.Serialize(message)), StringComparison.Ordinal);
    }

    [Fact]
    public async Task No_diagnostic_contains_the_token()
    {
        await using var harness = RealtimeHarness.Create();
        var connection = await harness.ConnectAsync(token: Token);

        await connection.SendRawAsync("not json at all");
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Closed, "the closed state");

        Assert.NotEmpty(harness.Diagnostics);
        Assert.DoesNotContain(harness.Diagnostics, line => line.Contains(Token, StringComparison.Ordinal));
    }

    [Fact]
    public async Task The_token_source_is_asked_once_per_connection_attempt()
    {
        await using var harness = RealtimeHarness.Create();
        var requests = 0;
        var connect = harness.Client.ConnectAsync(
            _ =>
            {
                Interlocked.Increment(ref requests);
                return ValueTask.FromResult<string?>(Token);
            },
            CancellationToken.None);

        var first = await harness.Server.WaitForConnectionAsync(0);
        await first.ReadAuthAsync();
        await first.SendReadyAsync();
        await connect.WithTimeoutAsync("the first ready");
        Assert.Equal(1, requests);

        await first.CloseAsync(1001);
        await Wait.UntilAsync(() => harness.Client.State == RealtimeConnectionState.Reconnecting, "the reconnect wait");
        await harness.Clock.AdvanceWhenTimerDueAsync(TimeSpan.FromSeconds(1), "the reconnect delay");
        var second = await harness.Server.WaitForConnectionAsync(1);
        Assert.Equal(Token, await second.ReadAuthAsync());
        Assert.Equal(2, requests);

        // The client holds no credential of its own: it asks for one per attempt.
        Assert.DoesNotContain(harness.Diagnostics, line => line.Contains(Token, StringComparison.Ordinal));
    }
}
