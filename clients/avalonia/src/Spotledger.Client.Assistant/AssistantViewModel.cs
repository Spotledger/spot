using System.Collections.ObjectModel;
using System.Windows.Input;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Assistant;

/// <summary>
/// The state behind the docked assistant panel: transcript, context chips, privacy
/// indicator, budget stop, offline mode and escalation. It holds no authority; every
/// action is a call to <see cref="IAssistantServer"/> as the signed-in user.
/// </summary>
public sealed class AssistantViewModel : ObservableObject
{
    /// <summary>Narrowest and widest the dock may be resized to, in device-independent pixels.</summary>
    public const int MinPanelWidth = 280;

    /// <inheritdoc cref="MinPanelWidth"/>
    public const int MaxPanelWidth = 720;

    /// <summary>Longest question the panel sends, in characters.</summary>
    public const int MaxInputLength = 4000;

    /// <summary>Most characters one assistant reply may add to the transcript; the rest is dropped.</summary>
    public const int MaxReplyLength = 60000;

    /// <summary>Most transcript entries kept; the oldest are dropped.</summary>
    public const int MaxTranscriptEntries = 400;

    private readonly IAssistantServer _server;
    private readonly Func<long> _nowUnixMs;
    private readonly Func<string, bool> _isRegistered;
    private readonly OfflineCatalogue _catalogue;
    private string _input = string.Empty;
    private bool _isOpen;
    private bool _isBusy;
    private bool _isOffline;
    private int _panelWidth = 380;
    private string? _budgetBanner;
    private string? _lastErrorCode;
    private string? _lastErrorText;
    private AiEndpointInfo? _endpoint;
    private DiagnosticBundle? _pendingBundle;
    private CancellationTokenSource? _cts;

    /// <summary>Creates the view model.</summary>
    /// <param name="server">The support-agent port.</param>
    /// <param name="nowUnixMs">The clock, injected; the model never reads the wall clock itself.</param>
    /// <param name="isRegisteredDestination">Whether a link target is a registered destination.</param>
    /// <param name="catalogue">The offline catalogue; the built-in one when null.</param>
    public AssistantViewModel(
        IAssistantServer server,
        Func<long> nowUnixMs,
        Func<string, bool> isRegisteredDestination,
        OfflineCatalogue? catalogue = null)
    {
        _server = server ?? throw new ArgumentNullException(nameof(server));
        _nowUnixMs = nowUnixMs ?? throw new ArgumentNullException(nameof(nowUnixMs));
        _isRegistered = isRegisteredDestination ?? throw new ArgumentNullException(nameof(isRegisteredDestination));
        _catalogue = catalogue ?? OfflineCatalogue.BuiltIn;
        SendCommand = new AsyncRelayCommand(_ => SendAsync(CancellationToken.None), _ => CanSend);
        ToggleCommand = new RelayCommand(_ => IsOpen = !IsOpen);
        RemoveChipCommand = new RelayCommand(p => { if (p is ContextChip c) { RemoveChip(c); } });
        CancelCommand = new RelayCommand(_ => CancelStream());
    }

    /// <summary>Whether <paramref name="destination"/> is a registered link target.</summary>
    public bool IsRegisteredDestination(string destination) => _isRegistered(destination);

    /// <summary>The transcript.</summary>
    public ObservableCollection<TranscriptEntry> Transcript { get; } = [];

    /// <summary>The chips that will be sent; the user can remove each.</summary>
    public ObservableCollection<ContextChip> Chips { get; } = [];

    /// <summary>Sends the input.</summary>
    public ICommand SendCommand { get; }

    /// <summary>Opens or closes the dock (keyboard toggle and command palette entry).</summary>
    public ICommand ToggleCommand { get; }

    /// <summary>Removes a chip (parameter: the chip).</summary>
    public ICommand RemoveChipCommand { get; }

    /// <summary>Stops a running reply.</summary>
    public ICommand CancelCommand { get; }

    /// <summary>Whether the dock is open.</summary>
    public bool IsOpen { get => _isOpen; set => SetField(ref _isOpen, value); }

    /// <summary>The dock width, clamped to the allowed range.</summary>
    public int PanelWidth
    {
        get => _panelWidth;
        set => SetField(ref _panelWidth, Math.Clamp(value, MinPanelWidth, MaxPanelWidth));
    }

    /// <summary>The text being typed.</summary>
    public string Input
    {
        get => _input;
        set { if (SetField(ref _input, value)) { OnPropertyChanged(nameof(CanSend)); } }
    }

    /// <summary>True while a reply streams.</summary>
    public bool IsBusy { get => _isBusy; private set { if (SetField(ref _isBusy, value)) { OnPropertyChanged(nameof(CanSend)); } } }

    /// <summary>True when AI is unavailable and the offline catalogue answers.</summary>
    public bool IsOffline { get => _isOffline; private set => SetField(ref _isOffline, value); }

    /// <summary>The budget-stop banner text, or null.</summary>
    public string? BudgetBanner
    {
        get => _budgetBanner;
        private set { if (SetField(ref _budgetBanner, value)) { OnPropertyChanged(nameof(CanSend)); } }
    }

    /// <summary>The endpoint indicator.</summary>
    public AiEndpointInfo? Endpoint { get => _endpoint; private set { if (SetField(ref _endpoint, value)) { OnPropertyChanged(nameof(PrivacyText)); } } }

    /// <summary>One line telling the user which model runs and whether data leaves the tenant.</summary>
    public string PrivacyText => Endpoint is null
        ? (IsOffline ? "Offline: built-in explanations only" : "AI endpoint unknown")
        : $"{Endpoint.Model} at {Endpoint.Endpoint}; " + (Endpoint.DataLeavesTenant ? "data leaves your tenant" : "data stays in your tenant");

    /// <summary>The redacted bundle waiting for the user's review, or null.</summary>
    public DiagnosticBundle? PendingBundle { get => _pendingBundle; private set => SetField(ref _pendingBundle, value); }

    /// <summary>Whether Send is enabled.</summary>
    public bool CanSend => !IsBusy && BudgetBanner is null && !string.IsNullOrWhiteSpace(Input);

    /// <summary>Loads the endpoint indicator from the server.</summary>
    public async Task RefreshEndpointAsync(CancellationToken ct) => Endpoint = await _server.GetEndpointAsync(ct);

    /// <summary>Replaces the screen, document and error chips from the current context.</summary>
    public void SetContext(string? screen, string? documentId, string? lastErrorCode, string? lastErrorText)
    {
        Chips.Clear();
        if (!string.IsNullOrEmpty(screen)) { Chips.Add(new ContextChip("screen", "Screen: " + screen, screen)); }
        if (!string.IsNullOrEmpty(documentId)) { Chips.Add(new ContextChip("document", "Document: " + documentId, documentId)); }
        if (!string.IsNullOrEmpty(lastErrorCode)) { Chips.Add(new ContextChip("error", "Last error: " + lastErrorCode, lastErrorCode)); }
        _lastErrorCode = lastErrorCode;
        _lastErrorText = lastErrorText;
    }

    /// <summary>Removes one chip so it is not sent.</summary>
    public void RemoveChip(ContextChip chip) => Chips.Remove(chip);

    /// <summary>Marks AI as unavailable (for example no connection or no AI configured).</summary>
    public void SetOffline(bool offline)
    {
        IsOffline = offline;
        OnPropertyChanged(nameof(PrivacyText));
    }

    /// <summary>Cancels the running reply.</summary>
    public void CancelStream() => _cts?.Cancel();

    /// <summary>Sends the input with the current chips and streams the reply into the transcript.</summary>
    public async Task SendAsync(CancellationToken ct)
    {
        if (!CanSend)
        {
            return;
        }

        var text = Input.Trim();
        var request = new AssistantRequest(text.Length > MaxInputLength ? text[..MaxInputLength] : text, [.. Chips]);
        Transcript.Add(new UserEntry(request.Text, request.Chips));
        Input = string.Empty;

        if (IsOffline)
        {
            Transcript.Add(new NoticeEntry(_catalogue.Explain(_lastErrorCode)));
            return;
        }

        TrimTranscript();
        IsBusy = true;
        _cts = CancellationTokenSource.CreateLinkedTokenSource(ct);
        AssistantEntry? current = null;
        try
        {
            await foreach (var ev in _server.AskAsync(request, _cts.Token))
            {
                switch (ev)
                {
                    case TextDelta d:
                        if (current is null)
                        {
                            current = new AssistantEntry(_isRegistered);
                            Transcript.Add(current);
                        }

                        current.Append(d.Text, MaxReplyLength);
                        break;
                    case ToolEvent t:
                        current = null;
                        Transcript.Add(new ToolEntry(t.Activity));
                        break;
                    case SystemFact f:
                        current = null;
                        Transcript.Add(new FactEntry(f.Text));
                        break;
                    case FixProposed p:
                        current = null;
                        var flow = new FixFlow(p.Plan, _server, _nowUnixMs);
                        Transcript.Add(new FixEntry(flow));
                        flow.MarkPlanShown();
                        break;
                    case BudgetStopped b:
                        BudgetBanner = b.Message;
                        break;
                    case AiUnavailable u:
                        SetOffline(true);
                        Transcript.Add(new NoticeEntry(u.Reason + " " + _catalogue.Explain(_lastErrorCode)));
                        break;
                }
            }
        }
        catch (OperationCanceledException)
        {
            Transcript.Add(new NoticeEntry("Stopped."));
        }
        catch (Exception ex) when (ex is HttpRequestException or System.IO.IOException or System.Text.Json.JsonException)
        {
            SetOffline(true);
            Transcript.Add(new NoticeEntry("The assistant could not be reached. " + _catalogue.Explain(_lastErrorCode)));
        }
        finally
        {
            IsBusy = false;
            _cts.Dispose();
            _cts = null;
        }
    }

    private void TrimTranscript()
    {
        while (Transcript.Count > MaxTranscriptEntries)
        {
            Transcript.RemoveAt(0);
        }
    }

    /// <summary>Builds the redacted diagnostic bundle for the user to review; nothing is sent yet.</summary>
    public void PrepareHumanRequest() => PendingBundle = DiagnosticRedactor.Build(Chips, _lastErrorText);

    /// <summary>Sends the reviewed bundle as a support request.</summary>
    public async Task SendHumanRequestAsync(string summary, CancellationToken ct)
    {
        var bundle = PendingBundle ?? throw new InvalidOperationException("Review the diagnostic bundle first.");
        var id = await _server.CreateSupportRequestAsync(DiagnosticRedactor.Redact(summary.Length > 500 ? summary[..500] : summary), bundle, ct);
        PendingBundle = null;
        Transcript.Add(new NoticeEntry("Support request " + id + " was created."));
    }

    /// <summary>Drops the bundle without sending it.</summary>
    public void DiscardHumanRequest() => PendingBundle = null;

    /// <summary>The user's edit of the bundle text before sending.</summary>
    public void EditBundle(string text) => PendingBundle = new DiagnosticBundle(DiagnosticRedactor.Redact(text));
}
