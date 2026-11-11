using Xunit;

namespace Spotledger.Client.Realtime.Tests;

/// <summary>A log of what one subscription received, safe to read from a test thread.</summary>
internal sealed class MessageLog
{
    private readonly object _gate = new();
    private readonly List<RealtimeRoomMessage> _messages = [];

    /// <summary>Collects a message; the shape of a subscriber handler.</summary>
    public void Add(RealtimeRoomMessage message)
    {
        lock (_gate)
        {
            _messages.Add(message);
        }
    }

    /// <summary>A snapshot in delivery order.</summary>
    public IReadOnlyList<RealtimeRoomMessage> Snapshot()
    {
        lock (_gate)
        {
            return [.. _messages];
        }
    }

    /// <summary>How many messages were delivered so far.</summary>
    public int Count => Snapshot().Count;

    /// <summary>The events delivered, in order.</summary>
    public IReadOnlyList<RealtimeEvent> Events => [.. Snapshot().OfType<RealtimeEvent>()];

    /// <summary>The event seq numbers delivered, in order.</summary>
    public IReadOnlyList<long> Seqs => [.. Events.Select(static e => e.Seq)];

    /// <summary>The single delivered message of the requested type.</summary>
    /// <typeparam name="T">The message type.</typeparam>
    public T Single<T>()
        where T : RealtimeRoomMessage => Assert.Single(Snapshot().OfType<T>());
}

/// <summary>The client under test, its scripted server and its manual clock.</summary>
internal sealed class RealtimeHarness : IAsyncDisposable
{
    private readonly object _gate = new();
    private readonly List<string> _diagnostics;

    private RealtimeHarness(ManualTimeProvider clock, ScriptedServer server, RealtimeClient client, List<string> diagnostics)
    {
        Clock = clock;
        Server = server;
        Client = client;
        _diagnostics = diagnostics;
    }

    /// <summary>The manual clock the client and the tests share.</summary>
    public ManualTimeProvider Clock { get; }

    /// <summary>The scripted server.</summary>
    public ScriptedServer Server { get; }

    /// <summary>The client under test.</summary>
    public RealtimeClient Client { get; }

    /// <summary>Every redacted diagnostic the client produced.</summary>
    public IReadOnlyList<string> Diagnostics
    {
        get
        {
            lock (_gate)
            {
                return [.. _diagnostics];
            }
        }
    }

    /// <summary>Builds a harness with the limits a test needs.</summary>
    public static RealtimeHarness Create(int handlerQueueCapacity = 256, int maxRooms = 200, int seed = 7)
    {
        var clock = new ManualTimeProvider();
        var server = new ScriptedServer();
        var diagnostics = new List<string>();
        var client = new RealtimeClient(
            new Uri("https://erp.example.test"),
            new RealtimeClientOptions
            {
                TransportFactory = server.Accept,
                TimeProvider = clock,
                Jitter = new Random(seed),
                HandlerQueueCapacity = handlerQueueCapacity,
                MaxRooms = maxRooms,
                DiagnosticLog = message =>
                {
                    lock (diagnostics)
                    {
                        diagnostics.Add(message);
                    }
                },
            });

        return new RealtimeHarness(clock, server, client, diagnostics);
    }

    /// <summary>Connects, answers <c>auth</c> with <c>ready</c>, and returns the ready connection.</summary>
    public async Task<ScriptedServer.Connection> ConnectAsync(int heartbeatSeconds = 25, string token = "session-token-under-test")
    {
        var connect = Client.ConnectAsync(token, CancellationToken.None);
        var connection = await Server.WaitForConnectionAsync(0).ConfigureAwait(false);
        await connection.ReadAuthAsync().ConfigureAwait(false);
        await connection.SendReadyAsync(heartbeatSeconds).ConfigureAwait(false);
        await connect.WithTimeoutAsync("the client to become ready").ConfigureAwait(false);
        Assert.Equal(RealtimeConnectionState.Ready, Client.State);
        return connection;
    }

    /// <inheritdoc/>
    public async ValueTask DisposeAsync()
    {
        await Client.DisposeAsync().ConfigureAwait(false);
    }
}
