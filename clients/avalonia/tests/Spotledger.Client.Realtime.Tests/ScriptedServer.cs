using System.Text;
using System.Text.Json;
using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>A bounded, condition-driven wait: the only "waiting" any test does.</summary>
internal static class Wait
{
    /// <summary>Polls <paramref name="condition"/> until it holds or the bounded wait expires.</summary>
    /// <param name="condition">What must become true.</param>
    /// <param name="because">What the test was waiting for, for the failure message.</param>
    /// <param name="timeoutMilliseconds">The bound; never an expectation about timing.</param>
    public static async Task UntilAsync(Func<bool> condition, string because, int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(condition);
        var watch = System.Diagnostics.Stopwatch.StartNew();
        while (!condition())
        {
            if (watch.ElapsedMilliseconds > timeoutMilliseconds)
            {
                throw new TimeoutException($"Timed out waiting for {because}.");
            }

            await Task.Delay(2).ConfigureAwait(false);
        }
    }

    /// <summary>Waits for a task with a bound, so a stuck client fails a test instead of hanging it.</summary>
    /// <typeparam name="T">The task's result.</typeparam>
    /// <param name="task">The task to await.</param>
    /// <param name="because">What the test was waiting for.</param>
    /// <param name="timeoutMilliseconds">The bound.</param>
    public static async Task<T> WithTimeoutAsync<T>(this Task<T> task, string because, int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(task);
        var completed = await Task.WhenAny(task, Task.Delay(timeoutMilliseconds)).ConfigureAwait(false);
        if (completed != task)
        {
            throw new TimeoutException($"Timed out waiting for {because}.");
        }

        return await task.ConfigureAwait(false);
    }

    /// <summary>Waits for a task with a bound.</summary>
    /// <param name="task">The task to await.</param>
    /// <param name="because">What the test was waiting for.</param>
    /// <param name="timeoutMilliseconds">The bound.</param>
    public static async Task WithTimeoutAsync(this Task task, string because, int timeoutMilliseconds = 5000)
    {
        ArgumentNullException.ThrowIfNull(task);
        var completed = await Task.WhenAny(task, Task.Delay(timeoutMilliseconds)).ConfigureAwait(false);
        if (completed != task)
        {
            throw new TimeoutException($"Timed out waiting for {because}.");
        }

        await task.ConfigureAwait(false);
    }
}

/// <summary>
/// A scripted fake server: it accepts one in-memory connection per client attempt and lets a
/// test say exactly what the server sends and assert exactly what it received. It fails the
/// test if the client sends anything before <c>ready</c>, which is the handshake order rule.
/// </summary>
internal sealed class ScriptedServer
{
    private readonly object _gate = new();
    private readonly List<Connection> _connections = [];

    /// <summary>The transport factory handed to <see cref="RealtimeClientOptions.TransportFactory"/>.</summary>
    public IRealtimeTransport Accept()
    {
        var pair = InMemoryRealtimeTransport.CreatePair();
        lock (_gate)
        {
            _connections.Add(new Connection(pair.Server));
        }

        return pair.Client;
    }

    /// <summary>How many connection attempts the client has made.</summary>
    public int ConnectionCount
    {
        get
        {
            lock (_gate)
            {
                return _connections.Count;
            }
        }
    }

    /// <summary>The most recent connection.</summary>
    public Connection Current
    {
        get
        {
            lock (_gate)
            {
                return _connections.Count == 0
                    ? throw new InvalidOperationException("The client has not connected yet.")
                    : _connections[^1];
            }
        }
    }

    /// <summary>Waits until the client's nth (zero-based) connection attempt arrives.</summary>
    /// <param name="index">The attempt index.</param>
    public async Task<Connection> WaitForConnectionAsync(int index)
    {
        await Wait.UntilAsync(() => ConnectionCount > index, $"connection attempt {index + 1}").ConfigureAwait(false);
        lock (_gate)
        {
            return _connections[index];
        }
    }

    /// <summary>One accepted connection.</summary>
    public sealed class Connection
    {
        private readonly List<JsonElement> _received = [];

        public Connection(InMemoryRealtimeTransport transport)
        {
            Transport = transport;
        }

        /// <summary>The server end of the in-memory pair.</summary>
        public InMemoryRealtimeTransport Transport { get; }

        /// <summary>True once <c>ready</c> was sent: before that only <c>auth</c> is legal.</summary>
        public bool ReadySent { get; private set; }

        /// <summary>Every message the client sent on this connection, in order.</summary>
        public IReadOnlyList<JsonElement> Received
        {
            get
            {
                lock (_received)
                {
                    return [.. _received];
                }
            }
        }

        /// <summary>The <c>type</c> of every message received, in order.</summary>
        public IReadOnlyList<string> ReceivedTypes => [.. Received.Select(static message => message.GetProperty("type").GetString() ?? "")];

        /// <summary>Reads the next message, enforcing that only <c>auth</c> precedes <c>ready</c>.</summary>
        public async Task<JsonElement> ReadAsync(int timeoutMilliseconds = 5000)
        {
            var frame = await Transport.ReceiveAsync(CancellationToken.None).WithTimeoutAsync(
                $"a client message (received: {string.Join(",", ReceivedTypes)})",
                timeoutMilliseconds).ConfigureAwait(false);
            if (frame.Kind != RealtimeFrameKind.Text)
            {
                throw new InvalidOperationException($"The client sent a {frame.Kind} frame, which the contract forbids.");
            }

            using var document = JsonDocument.Parse(frame.Payload);
            var message = document.RootElement.Clone();
            var type = message.GetProperty("type").GetString() ?? "";
            if (!ReadySent && !string.Equals(type, "auth", StringComparison.Ordinal))
            {
                throw new InvalidOperationException($"The client sent '{type}' before ready; only auth may precede ready.");
            }

            lock (_received)
            {
                _received.Add(message);
            }

            return message;
        }

        /// <summary>Reads the <c>auth</c> message and returns its token.</summary>
        public async Task<string> ReadAuthAsync()
        {
            var message = await ReadAsync().ConfigureAwait(false);
            Assert.Equal("auth", message.GetProperty("type").GetString());
            return message.GetProperty("token").GetString() ?? "";
        }

        /// <summary>Asserts that nothing at all was sent before <c>ready</c>.</summary>
        public Task AssertNothingElseSentAsync() =>
            AssertNoMessageAsync("the client to stay silent before ready");

        /// <summary>Asserts the client sends nothing (a bounded negative probe, never a sleep).</summary>
        /// <param name="because">What the silence is about.</param>
        public async Task AssertNoMessageAsync(string because)
        {
            try
            {
                var frame = await Transport.ReceiveAsync(CancellationToken.None)
                    .WithTimeoutAsync(because, timeoutMilliseconds: 100)
                    .ConfigureAwait(false);
                throw new InvalidOperationException($"The client sent a {frame.Kind} frame but {because}.");
            }
            catch (TimeoutException)
            {
                // The expected outcome: nothing arrived.
            }
        }

        /// <summary>Sends raw JSON as one text frame.</summary>
        public Task SendRawAsync(string json) =>
            Transport.SendTextAsync(Encoding.UTF8.GetBytes(json), CancellationToken.None);

        /// <summary>Sends a raw text frame from bytes (for oversized frames).</summary>
        public Task SendRawAsync(ReadOnlyMemory<byte> utf8) =>
            Transport.SendTextAsync(utf8, CancellationToken.None);

        /// <summary>Sends <c>ready</c> and marks the handshake complete.</summary>
        public async Task SendReadyAsync(int heartbeatSeconds = 25, string user = "0192f3c4-5b6a-7c8d-9e0f-00000000aaaa")
        {
            await SendRawAsync(
                $"{{\"type\":\"ready\",\"user\":\"{user}\",\"heartbeat_seconds\":{heartbeatSeconds}}}").ConfigureAwait(false);
            ReadySent = true;
        }

        /// <summary>Sends a <c>joined</c> reply for the join the client just sent.</summary>
        public async Task<JsonElement> SendJoinedForAsync(string outcome = "joined", int? replayed = null)
        {
            var join = await ReadAsync().ConfigureAwait(false);
            Assert.Equal("join", join.GetProperty("type").GetString());
            var room = join.GetProperty("room").GetString();
            var roomRef = join.GetProperty("ref").GetString();
            await SendJoinedAsync(room!, roomRef!, outcome, replayed).ConfigureAwait(false);
            return join;
        }

        /// <summary>Sends a <c>joined</c> reply.</summary>
        public Task SendJoinedAsync(string room, string roomReference, string outcome, int? replayed = null)
        {
            var replayedPart = replayed is { } count ? $",\"replayed\":{count}" : "";
            return SendRawAsync(
                $"{{\"type\":\"joined\",\"ref\":{Quote(roomReference)},\"room\":{Quote(room)},\"outcome\":{Quote(outcome)}{replayedPart}}}");
        }

        /// <summary>Sends a <c>join_error</c> reply.</summary>
        public Task SendJoinErrorAsync(string room, string roomReference, string code, string message = "Refused.") =>
            SendRawAsync(
                $"{{\"type\":\"join_error\",\"ref\":{Quote(roomReference)},\"room\":{Quote(room)},\"error\":{{\"code\":{Quote(code)},\"message\":{Quote(message)}}}}}");

        /// <summary>Sends an <c>event</c>.</summary>
        public Task SendEventAsync(
            string room,
            long seq,
            string kind = "doc_updated",
            string payload = "{\"doctype\":\"sales_invoice\",\"id\":\"0192f3c4-5b6a-7c8d-9e0f-1a2b3c4d5e6f\",\"version\":4}",
            string publishedAt = "2026-09-29T10:15:02.120Z") =>
            SendRawAsync(
                $"{{\"type\":\"event\",\"room\":{Quote(room)},\"seq\":{seq},\"kind\":{Quote(kind)},\"payload\":{payload},\"published_at\":{Quote(publishedAt)}}}");

        /// <summary>Sends a WebSocket ping, as the contract's server does every heartbeat.</summary>
        public Task SendPingAsync() => Transport.SendPingAsync(CancellationToken.None);

        /// <summary>Sends a binary frame (the contract says the socket closes with 1003).</summary>
        public Task SendBinaryAsync() => Transport.SendBinaryAsync(new byte[] { 0x01, 0x02 }, CancellationToken.None);

        /// <summary>Closes the connection with a contract code.</summary>
        public Task CloseAsync(int closeCode) => Transport.CloseAsync(closeCode, null, CancellationToken.None);

        /// <summary>Kills the connection with no close frame, like a dropped network.</summary>
        public void Abort() => Transport.Abort();

        private static string Quote(string value) => JsonSerializer.Serialize(value);
    }
}
