namespace Spotledger.AdminCenter.Tenants;

/// <summary>
/// Desired state for a group: the settings editor generated from the schema (one row per setting,
/// no per-setting UI code), the drift list with single and bulk remediation, the wave configuration,
/// and the rollout with its halted banner. A halted rollout blocks the next wave until it is resumed.
/// </summary>
public sealed class DesiredStateScreen : TenantScreen
{
    private readonly string _group;
    private readonly Dictionary<string, string> _draft = new(StringComparer.Ordinal);
    private readonly HashSet<string> _selectedDrift = new(StringComparer.Ordinal);
    private readonly List<WaveConfig> _waves = [];
    private IReadOnlyList<SettingDescriptor> _schema = [];
    private IReadOnlyDictionary<string, SettingValue> _values = new Dictionary<string, SettingValue>();
    private IReadOnlyList<DriftItem> _drift = [];
    private RolloutStatus? _rollout;
    private SettingsScreenModel _editor = new([], new Dictionary<string, SettingValue>());

    /// <summary>Creates the screen for one group.</summary>
    public DesiredStateScreen(TenantContext context, string group)
        : base(context)
    {
        _group = group ?? throw new ArgumentNullException(nameof(group));
    }

    /// <inheritdoc />
    public override string Id => TenantTools.DesiredStateId;

    /// <inheritdoc />
    public override string Title => "Desired state and drift";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Settings", "Save", "Drift", "Remediate", "Waves", "StartNextWave", "Resume"];

    /// <summary>The settings editor rows (search, differs-from-default, inheritance, server errors).</summary>
    public SettingsScreenModel Editor => _editor;

    /// <summary>Unsaved edits, by key.</summary>
    public IReadOnlyDictionary<string, string> Draft => _draft;

    /// <summary>Drifted items.</summary>
    public IReadOnlyList<DriftItem> Drift => _drift;

    /// <summary>Drift ids ticked for bulk remediation.</summary>
    public IReadOnlyCollection<string> SelectedDrift => _selectedDrift;

    /// <summary>The wave configuration being edited.</summary>
    public IReadOnlyList<WaveConfig> Waves => _waves;

    /// <summary>The rollout, or null.</summary>
    public RolloutStatus? Rollout => _rollout;

    /// <summary>The halted-rollout banner; null when the rollout runs or none exists.</summary>
    public string? HaltedBanner => _rollout?.Paused is { } pause
        ? $"The rollout is halted: {pause.Detail}{(pause.Node is null ? string.Empty : $" (node {pause.Node})")}. No further wave starts until you resume."
        : null;

    /// <summary>True when the next wave may start: a rollout exists, is not halted and has waves left.</summary>
    public bool CanStartNextWave => _rollout is { Paused: null } rollout && rollout.CompletedWaves < rollout.Waves.Count;

    /// <summary>True when Resume works: only a halted rollout.</summary>
    public bool CanResume => _rollout?.Paused is not null;

    /// <summary>Edits a setting. The value is validated by the server on save.</summary>
    public void Set(string key, string value)
    {
        if (!_schema.Any(s => string.Equals(s.Key, key, StringComparison.Ordinal)))
        {
            Error = $"{key} is not a setting.";
            return;
        }

        _draft[key] = value;
        OnPropertyChanged(nameof(Draft));
    }

    /// <summary>Saves the draft. Per-key errors from the server stay on their rows and nothing is applied.</summary>
    public async Task<bool> SaveAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (_draft.Count == 0)
        {
            Error = "There is nothing to save.";
            return false;
        }

        IReadOnlyDictionary<string, string> errors;
        try
        {
            errors = await Api.SetDesiredAsync(_group, new Dictionary<string, string>(_draft), cancellationToken);
        }
        catch (TenantApiException refused)
        {
            Error = refused.Message;
            return false;
        }

        _editor.SetServerErrors(errors);
        OnPropertyChanged(nameof(Editor));
        if (errors.Count > 0)
        {
            Error = "Some values were refused. Fix them and save again.";
            return false;
        }

        _draft.Clear();
        OnPropertyChanged(nameof(Draft));
        return await LoadAsync(nowUnixSeconds, cancellationToken);
    }

    /// <summary>Ticks or unticks a drift item for bulk remediation. Exempt items cannot be ticked.</summary>
    public bool ToggleDrift(string id)
    {
        var item = _drift.FirstOrDefault(d => string.Equals(d.Id, id, StringComparison.Ordinal));
        if (item is null || item.Exempt)
        {
            return false;
        }

        if (!_selectedDrift.Remove(id))
        {
            _selectedDrift.Add(id);
        }

        OnPropertyChanged(nameof(SelectedDrift));
        return true;
    }

    /// <summary>Starts remediating one drifted item.</summary>
    public bool BeginRemediate(string id)
    {
        var item = _drift.FirstOrDefault(d => string.Equals(d.Id, id, StringComparison.Ordinal));
        if (item is null || item.Exempt)
        {
            Error = "That item is exempt from the desired state.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.DriftRemediate, ("group", _group), ("items", item.Id)), $"Set {item.Setting} on {item.Subject} back to {item.Desired}.");
        return true;
    }

    /// <summary>Starts remediating every ticked item as one bulk action (exempt items are never included).</summary>
    public bool BeginRemediateSelected()
    {
        var chosen = _drift.Where(d => !d.Exempt && _selectedDrift.Contains(d.Id)).OrderBy(d => d.Id, StringComparer.Ordinal).ToList();
        var ids = chosen.Select(d => d.Id).ToList();
        if (ids.Count == 0)
        {
            Error = "Tick at least one drifted item first.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.DriftRemediate, ("group", _group), ("items", string.Join(',', ids))), $"Bring {ids.Count} drifted item{(ids.Count == 1 ? string.Empty : "s")} back to the desired state: {string.Join("; ", chosen.Select(d => $"{d.Setting} on {d.Subject}"))}.");
        return true;
    }

    /// <summary>Adds a wave to the configuration. Names must be unique and sizes positive.</summary>
    public bool AddWave(string name, int maxNodes, ulong soakSeconds)
    {
        if (string.IsNullOrWhiteSpace(name) || maxNodes <= 0)
        {
            Error = "A wave needs a name and at least one node.";
            return false;
        }

        if (name.Any(c => c is ':' or ';' || char.IsControl(c)))
        {
            Error = "A wave name cannot contain a colon, a semicolon or control characters.";
            return false;
        }

        if (_waves.Any(w => string.Equals(w.Name, name.Trim(), StringComparison.Ordinal)))
        {
            Error = $"There is already a wave called {name.Trim()}.";
            return false;
        }

        _waves.Add(new WaveConfig(name.Trim(), maxNodes, soakSeconds, Canary: _waves.Count == 0));
        Error = null;
        OnPropertyChanged(nameof(Waves));
        return true;
    }

    /// <summary>Removes a wave; the first remaining wave becomes the canary.</summary>
    public bool RemoveWave(string name)
    {
        if (_waves.RemoveAll(w => string.Equals(w.Name, name, StringComparison.Ordinal)) == 0)
        {
            return false;
        }

        if (_waves.Count > 0)
        {
            _waves[0] = _waves[0] with { Canary = true };
            for (var i = 1; i < _waves.Count; i++)
            {
                _waves[i] = _waves[i] with { Canary = false };
            }
        }

        OnPropertyChanged(nameof(Waves));
        return true;
    }

    /// <summary>Starts saving the wave configuration.</summary>
    public bool BeginSaveWaves()
    {
        if (_waves.Count == 0)
        {
            Error = "Add at least one wave.";
            return false;
        }

        var text = string.Join(';', _waves.Select(w => $"{w.Name}:{w.MaxNodes}:{w.SoakSeconds}:{(w.Canary ? "canary" : "normal")}"));
        Begin(AdminAction.Create(TenantOps.RolloutConfigure, ("group", _group), ("waves", text)), $"Save {_waves.Count} wave{(_waves.Count == 1 ? string.Empty : "s")}; the first is the canary.");
        return true;
    }

    /// <summary>Starts the next wave. Refused while the rollout is halted.</summary>
    public bool BeginNextWave()
    {
        if (_rollout?.Paused is not null)
        {
            Error = "The rollout is halted. Resume it before starting another wave.";
            return false;
        }

        if (!CanStartNextWave || _rollout is null)
        {
            Error = "There is no wave left to start.";
            return false;
        }

        var next = _rollout.Waves[_rollout.CompletedWaves];
        Begin(AdminAction.Create(TenantOps.RolloutNextWave, ("rollout", _rollout.RolloutId), ("wave", next.Name)), $"Start wave {next.Name} (up to {next.MaxNodes} nodes).");
        return true;
    }

    /// <summary>Starts resuming a halted rollout.</summary>
    public bool BeginResume()
    {
        if (_rollout is not { Paused: not null } rollout)
        {
            Error = "The rollout is not halted.";
            return false;
        }

        Begin(AdminAction.Create(TenantOps.RolloutResume, ("rollout", rollout.RolloutId)), "Resume the halted rollout after the cause was dealt with.");
        return true;
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _schema = await Api.GetSettingsSchemaAsync(cancellationToken);
        _values = await Api.GetDesiredAsync(_group, cancellationToken);
        _drift = await Api.GetDriftAsync(_group, cancellationToken);
        _rollout = await Api.GetRolloutAsync(_group, cancellationToken);
        _editor = new SettingsScreenModel(_schema, _values);
        _selectedDrift.RemoveWhere(id => !_drift.Any(d => string.Equals(d.Id, id, StringComparison.Ordinal) && !d.Exempt));
        if (_rollout is not null && _waves.Count == 0)
        {
            _waves.AddRange(_rollout.Waves);
        }

        OnPropertyChanged(nameof(Editor));
        OnPropertyChanged(nameof(Drift));
        OnPropertyChanged(nameof(Rollout));
        OnPropertyChanged(nameof(Waves));
        OnPropertyChanged(nameof(HaltedBanner));
        OnPropertyChanged(nameof(CanStartNextWave));
        OnPropertyChanged(nameof(CanResume));
    }
}

/// <summary>
/// The fleet health grid: tenants and servers in the three-colour model, worst first, with filters.
/// It polls the telemetry feed at the interval from settings; the caller passes the clock, so a
/// failure that appears between polls shows on the next one, never later than one interval.
/// </summary>
public sealed class FleetHealthScreen : TenantScreen
{
    private IReadOnlyList<HealthRow> _rows = [];
    private HealthColor? _color;
    private bool? _tenantsOnly;
    private string _search = string.Empty;
    private uint _intervalSeconds = 30;
    private long _nextPollAt;

    /// <summary>Creates the screen.</summary>
    public FleetHealthScreen(TenantContext context)
        : base(context)
    {
    }

    /// <inheritdoc />
    public override string Id => TenantTools.FleetHealthId;

    /// <inheritdoc />
    public override string Title => "Fleet health";

    /// <inheritdoc />
    public override IReadOnlyList<string> FocusOrder => ["Search", "Colour", "Kind", "Grid", "Refresh"];

    /// <summary>Show only this colour, or null for all.</summary>
    public HealthColor? Color
    {
        get => _color;
        set
        {
            if (SetField(ref _color, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>True for tenants only, false for servers only, null for both.</summary>
    public bool? TenantsOnly
    {
        get => _tenantsOnly;
        set
        {
            if (SetField(ref _tenantsOnly, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>Text matched in names.</summary>
    public string Search
    {
        get => _search;
        set
        {
            if (SetField(ref _search, value))
            {
                OnPropertyChanged(nameof(Visible));
            }
        }
    }

    /// <summary>The poll interval in seconds.</summary>
    public uint PollIntervalSeconds => _intervalSeconds;

    /// <summary>The rows that pass the filters, worst colour first, then by name.</summary>
    public IReadOnlyList<HealthRow> Visible =>
    [
        .. _rows
            .Where(r => (Color is null || r.Color == Color)
                && (TenantsOnly is null || r.IsTenant == TenantsOnly)
                && (string.IsNullOrWhiteSpace(Search) || r.Name.Contains(Search, StringComparison.OrdinalIgnoreCase)))
            .OrderBy(r => r.Color)
            .ThenBy(r => r.Name, StringComparer.Ordinal),
    ];

    /// <summary>How many rows of each colour (all rows, before filters).</summary>
    public IReadOnlyDictionary<HealthColor, int> Counts =>
        Enum.GetValues<HealthColor>().ToDictionary(c => c, c => _rows.Count(r => r.Color == c));

    /// <summary>One line over the grid, e.g. "1 red, 2 amber, 5 green".</summary>
    public string Summary => $"{Counts[HealthColor.Red]} red, {Counts[HealthColor.Amber]} amber, {Counts[HealthColor.Green]} green";

    /// <summary>
    /// Polls when the interval has passed since the last poll (or never polled). Returns true when it
    /// fetched. A refused poll keeps the last rows and tries again at the next interval.
    /// </summary>
    public async Task<bool> PollAsync(long nowUnixSeconds, CancellationToken cancellationToken = default)
    {
        if (nowUnixSeconds < _nextPollAt)
        {
            return false;
        }

        _nextPollAt = nowUnixSeconds + _intervalSeconds;
        return await LoadAsync(nowUnixSeconds, cancellationToken);
    }

    /// <inheritdoc />
    protected override async Task FetchAsync(long nowUnixSeconds, CancellationToken cancellationToken)
    {
        _intervalSeconds = Math.Max(1, await Api.GetPollIntervalSecondsAsync(cancellationToken));
        _nextPollAt = nowUnixSeconds + _intervalSeconds;
        _rows = await Api.GetHealthAsync(cancellationToken);
        OnPropertyChanged(nameof(Visible));
        OnPropertyChanged(nameof(Counts));
        OnPropertyChanged(nameof(Summary));
        OnPropertyChanged(nameof(PollIntervalSeconds));
    }
}
