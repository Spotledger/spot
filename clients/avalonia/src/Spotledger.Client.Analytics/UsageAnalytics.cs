namespace Spotledger.Client.Analytics;

/// <summary>
/// Opt-in, privacy-safe usage analytics. Events are aggregated locally into counters keyed by
/// (event, screen id, action id / error code / shortcut id). Nothing else is ever stored: no
/// document data, no user text, no field values, no user id, no timestamps (the day is an
/// argument of the flush; the class never reads the clock).
/// <para>
/// With consent off every call is a no-op and <see cref="FlushAsync"/> sends nothing. Recording
/// never throws and never blocks the UI: an invalid id, an id the screens manifest does not list,
/// or a full table drops the event.
/// </para>
/// <para>
/// Withdrawal (consent on to off, or <see cref="Clear"/>) discards the stored install id as well as
/// the counters, so the next opt-in starts a new identity: nothing is linkable across an opt-out.
/// </para>
/// </summary>
public sealed class UsageAnalytics : IDisposable
{
    /// <summary>Distinct counters held at once; further new keys are dropped and counted.</summary>
    public const int MaxCounters = 2048;

    /// <summary>Upper bound of <see cref="RejectedCount"/>, so a hostile caller cannot inflate the payload.</summary>
    public const long MaxRejected = 1_000_000;

    private readonly IAnalyticsConsent _consent;
    private readonly IAnalyticsSink _sink;
    private readonly IInstallIdStore _installIds;
    private readonly IKnownScreens _knownScreens;
    private readonly Dictionary<(AnalyticsEvent Event, string? Screen, string? Item), long> _counters = [];
    private readonly object _gate = new();
    private long _dropped;
    private long _rejected;
    private bool _wasEnabled;
    private long _generation;
    private string? _installId;
    private CancellationTokenSource _withdrawal = new();

    /// <summary>The host's wiring: the store it persists the install id in and the manifest's screen ids.</summary>
    public UsageAnalytics(IAnalyticsConsent consent, IAnalyticsSink sink, IInstallIdStore installIds, IKnownScreens knownScreens)
    {
        ArgumentNullException.ThrowIfNull(consent);
        ArgumentNullException.ThrowIfNull(sink);
        ArgumentNullException.ThrowIfNull(installIds);
        ArgumentNullException.ThrowIfNull(knownScreens);
        _consent = consent;
        _sink = sink;
        _installIds = installIds;
        _knownScreens = knownScreens;
        _wasEnabled = consent.IsEnabled;
        _installId = installIds.Read();
        if (!consent.IsEnabled && _installId is not null)
        {
            // Consent is off at start-up, so whatever id is stored belongs to an opt-in that has
            // since ended (withdrawn while this instance was not running): never reuse it.
            _installId = null;
            installIds.Discard();
        }

        if (!InstallId.IsValid(_installId))
        {
            // A corrupt stored id is not reused; the next opt-in creates a new one.
            _installId = null;
            if (installIds.Read() is not null)
            {
                installIds.Discard();
            }
        }
    }

    /// <summary>
    /// A collector bound to one fixed install id in memory, for tests and for a host that persists
    /// the id itself. Rotation still applies to everything the store holds.
    /// </summary>
    public UsageAnalytics(IAnalyticsConsent consent, IAnalyticsSink sink, string installId, IKnownScreens knownScreens)
        : this(consent, sink, InMemoryInstallIdStore.Seeded(installId), knownScreens)
    {
    }

    /// <summary>Screen ids analytics has refused because the manifest does not list them (a number, never their text).</summary>
    public long RejectedCount
    {
        get
        {
            ObserveConsent();
            lock (_gate)
            {
                return _rejected;
            }
        }
    }

    /// <summary>Releases the withdrawal signal; the collected counters die with the instance.</summary>
    public void Dispose()
    {
        _withdrawal.Dispose();
        GC.SuppressFinalize(this);
    }

    /// <summary>A screen was opened.</summary>
    public bool ScreenOpened(string screenId) =>
        Record(AnalyticsEvent.ScreenOpened, screenId, null, KnownScreen(screenId));

    /// <summary>A named action on a screen was used (a toolbar button, a menu command).</summary>
    public bool ActionUsed(string screenId, string actionId) =>
        Record(AnalyticsEvent.ActionUsed, screenId, actionId, KnownScreen(screenId) && AnalyticsIds.IsValidId(actionId));

    /// <summary>An error was shown. Only the error code is kept, never the message.</summary>
    public bool ErrorShown(string code) =>
        Record(AnalyticsEvent.ErrorShown, null, code, AnalyticsIds.IsValidCode(code));

    /// <summary>A keyboard shortcut was used on a screen, named by its command id, not its keys.</summary>
    public bool ShortcutUsed(string screenId, string shortcutId) =>
        Record(AnalyticsEvent.ShortcutUsed, screenId, shortcutId, KnownScreen(screenId) && AnalyticsIds.IsValidId(shortcutId));

    /// <summary>
    /// The inspector API: exactly what the next flush would send, so the user can see it in
    /// settings. Empty when consent is off.
    /// </summary>
    public AnalyticsPayload PendingPayload(DateOnly day)
    {
        ObserveConsent();
        lock (_gate)
        {
            return _consent.IsEnabled
                ? Build(day, [.. _counters], _dropped, _rejected, EnsureInstallId())
                : Build(day, [], 0, 0, string.Empty);
        }
    }

    /// <summary>The pending payload as the JSON that would be posted.</summary>
    public string InspectJson(DateOnly day) => AnalyticsJson.Serialize(PendingPayload(day));

    /// <summary>
    /// Discards everything held and forgets the install id: this is the withdrawal path (the Shell
    /// calls it when the user turns analytics off or resets them). A batch being sent is cancelled
    /// and the next opt-in creates a new id with <see cref="InstallId.Create"/>.
    /// </summary>
    public void Clear()
    {
        CancellationTokenSource cancelled;
        lock (_gate)
        {
            _counters.Clear();
            _dropped = 0;
            _rejected = 0;
            _generation++;
            _installId = null;
            _installIds.Discard();
            cancelled = _withdrawal;
            _withdrawal = new CancellationTokenSource();
        }

        // Cancelled outside the lock: a continuation of the aborted send must not re-enter it.
        cancelled.Cancel();
    }

    /// <summary>
    /// Sends the pending counters as one batch. Consent off sends nothing and discards what was
    /// held (including the install id). A failed send keeps the counters; events recorded during the
    /// send are never lost, and a batch being sent when the user withdraws is cancelled and not
    /// counted as sent.
    /// </summary>
    public async Task<FlushResult> FlushAsync(DateOnly day, CancellationToken cancellationToken = default)
    {
        ObserveConsent();
        if (!_consent.IsEnabled)
        {
            Clear();
            return FlushResult.ConsentOff;
        }

        KeyValuePair<(AnalyticsEvent Event, string? Screen, string? Item), long>[] snapshot;
        long dropped;
        long rejected;
        long generation;
        string installId;
        CancellationTokenSource withdrawal;
        lock (_gate)
        {
            snapshot = [.. _counters];
            dropped = _dropped;
            rejected = _rejected;
            generation = _generation;
            withdrawal = _withdrawal;
            installId = EnsureInstallId();
        }

        if (!_consent.IsEnabled)
        {
            // Withdrawn between the two reads: nothing leaves the machine.
            Clear();
            return FlushResult.ConsentOff;
        }

        if (snapshot.Length == 0 && dropped == 0 && rejected == 0)
        {
            return FlushResult.NothingToSend;
        }

        var batch = Build(day, snapshot, dropped, rejected, installId);
        using var linked = CancellationTokenSource.CreateLinkedTokenSource(cancellationToken, withdrawal.Token);
        try
        {
            await _sink.SendAsync(batch, linked.Token).ConfigureAwait(false);
        }
        catch (OperationCanceledException) when (withdrawal.IsCancellationRequested && !cancellationToken.IsCancellationRequested)
        {
            // The user withdrew while the batch was in flight; Clear() already discarded it.
            return FlushResult.ConsentOff;
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (AnalyticsNoIngestTokenException)
        {
            // No anonymous credential for this deployment: nothing leaves the machine, counters stay.
            return FlushResult.NoIngestToken;
        }
        catch (AnalyticsInvalidIngestTokenException)
        {
            return FlushResult.NoIngestToken;
        }
        catch (AnalyticsTransportException)
        {
            // Non-https endpoint or a redirect: nothing was delivered, keep the batch for a fixed endpoint.
            return FlushResult.Failed;
        }
        catch (Exception)
        {
            // Analytics must never surface a failure to the user; the counters stay for the next flush.
            return FlushResult.Failed;
        }

        lock (_gate)
        {
            if (generation != _generation)
            {
                // Withdrawn and cleared during the send: there is nothing to subtract.
                return FlushResult.Sent;
            }

            foreach (var (key, sent) in snapshot)
            {
                if (_counters.TryGetValue(key, out var now))
                {
                    if (now <= sent)
                    {
                        _counters.Remove(key);
                    }
                    else
                    {
                        _counters[key] = now - sent;
                    }
                }
            }

            _dropped = Math.Max(0, _dropped - dropped);
            _rejected = Math.Max(0, _rejected - rejected);
        }

        return FlushResult.Sent;
    }

    /// <summary>
    /// Notices consent going from on to off, which is a withdrawal: counters and the install id are
    /// discarded and any batch in flight is cancelled. Polled on every entry point, because
    /// <see cref="IAnalyticsConsent"/> is a setting the user flips at any time.
    /// </summary>
    private void ObserveConsent()
    {
        var enabled = _consent.IsEnabled;
        if (enabled == _wasEnabled)
        {
            return;
        }

        _wasEnabled = enabled;
        if (!enabled)
        {
            Clear();
        }
    }

    private bool KnownScreen(string? screenId)
    {
        if (!AnalyticsIds.IsValidId(screenId))
        {
            // Not even an id shape: dropped, never stored, never echoed.
            return false;
        }

        if (_knownScreens.Contains(screenId))
        {
            return true;
        }

        lock (_gate)
        {
            // A count only. The rejected text is never kept, logged or sent.
            _rejected = _rejected >= MaxRejected ? MaxRejected : _rejected + 1;
        }

        return false;
    }

    private bool Record(AnalyticsEvent kind, string? screen, string? item, bool valid)
    {
        ObserveConsent();
        if (!_consent.IsEnabled || !valid)
        {
            return false;
        }

        var key = (kind, screen, item);
        lock (_gate)
        {
            if (_counters.TryGetValue(key, out var n))
            {
                _counters[key] = n == long.MaxValue ? n : n + 1;
                return true;
            }

            if (_counters.Count >= MaxCounters)
            {
                _dropped = _dropped == long.MaxValue ? _dropped : _dropped + 1;
                return false;
            }

            _counters[key] = 1;
            return true;
        }
    }

    /// <summary>Creates the install id on first use while consent is on; call under <see cref="_gate"/>.</summary>
    private string EnsureInstallId()
    {
        if (_installId is not null)
        {
            return _installId;
        }

        var stored = _installIds.Read();
        string id;
        if (stored is not null && InstallId.IsValid(stored))
        {
            id = stored;
        }
        else
        {
            id = InstallId.Create();
            _installIds.Store(id);
        }

        _installId = id;
        return id;
    }

    private static AnalyticsPayload Build(
        DateOnly day,
        IEnumerable<KeyValuePair<(AnalyticsEvent Event, string? Screen, string? Item), long>> counters,
        long dropped,
        long rejected,
        string? installId = null)
    {
        var list = counters
            .Select(kv => new AnalyticsCounter(kv.Key.Event, kv.Key.Screen, kv.Key.Item, kv.Value))
            .OrderBy(c => c.Event)
            .ThenBy(c => c.ScreenId, StringComparer.Ordinal)
            .ThenBy(c => c.ItemId, StringComparer.Ordinal)
            .ToList();
        return new AnalyticsPayload(AnalyticsPayload.CurrentSchemaVersion, installId ?? string.Empty, day, list, dropped, rejected);
    }
}
