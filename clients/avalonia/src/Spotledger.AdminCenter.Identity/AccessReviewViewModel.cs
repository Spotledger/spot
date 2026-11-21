using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// Access review: create a campaign, work a reviewer's list (keep or remove per assignment, with
/// the last-use column and separation-of-duties overrides marked), follow progress and escalation,
/// and close it with a completion report.
/// </summary>
public sealed class AccessReviewViewModel : IdentityViewModel
{
    private IReadOnlyList<CampaignInfo> _campaigns = [];
    private List<ReviewItem> _worklist = [];
    private readonly Dictionary<string, ReviewDecision> _decisions = [];
    private ReviewReport? _report;

    /// <summary>Creates the screen.</summary>
    public AccessReviewViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
    }

    /// <summary>The campaigns.</summary>
    public IReadOnlyList<CampaignInfo> Campaigns
    {
        get => _campaigns;
        private set => SetField(ref _campaigns, value);
    }

    /// <summary>The campaign being worked.</summary>
    public string? CampaignId { get; private set; }

    /// <summary>The reviewer's list.</summary>
    public IReadOnlyList<ReviewItem> Worklist => _worklist;

    /// <summary>The completion report, once closed.</summary>
    public ReviewReport? Report
    {
        get => _report;
        private set => SetField(ref _report, value);
    }

    /// <summary>How many items have a decision (locally or already on the server).</summary>
    public int Decided => _worklist.Count(i => DecisionOf(i) != ReviewDecision.Undecided);

    /// <summary>How many items there are.</summary>
    public int Total => _worklist.Count;

    /// <summary>Progress as text (<c>3 of 10 decided</c>).</summary>
    public string Progress => $"{Decided} of {Total} decided";

    /// <summary>The current decision on an item.</summary>
    public ReviewDecision DecisionOf(ReviewItem item)
    {
        ArgumentNullException.ThrowIfNull(item);
        return _decisions.TryGetValue(item.Id, out var local) ? local : item.Decision;
    }

    /// <summary>The escalation status of the open campaign, in words; null when none.</summary>
    public string? Escalation => Campaigns.FirstOrDefault(c => c.Id == CampaignId)?.Escalation;

    /// <summary>Loads the campaign list.</summary>
    public async Task RefreshAsync()
    {
        var campaigns = await CallAsync(() => Gateway.ListCampaignsAsync());
        if (campaigns is not null)
        {
            Campaigns = campaigns;
            OnPropertyChanged(nameof(Escalation));
        }
    }

    /// <summary>Creates a campaign. Dates are checked against <paramref name="today"/>.</summary>
    public async Task<bool> CreateCampaignAsync(CampaignRequest request, DateOnly today)
    {
        ArgumentNullException.ThrowIfNull(request);
        if (string.IsNullOrWhiteSpace(request.Name))
        {
            Error = "Name the campaign.";
            return false;
        }

        if (request.ReviewerIds.Count == 0)
        {
            Error = "Choose at least one reviewer.";
            return false;
        }

        if (request.DueDate <= today)
        {
            Error = "The due date must be in the future.";
            return false;
        }

        var created = await CallAsync(() => Gateway.CreateCampaignAsync(request));
        if (created is null)
        {
            return false;
        }

        Status = $"Campaign {created.Name} started.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Opens a campaign's worklist.</summary>
    public async Task<bool> OpenAsync(string campaignId)
    {
        var items = await CallAsync(() => Gateway.GetWorklistAsync(campaignId));
        if (items is null)
        {
            return false;
        }

        CampaignId = campaignId;
        _worklist = [.. items];
        _decisions.Clear();
        Report = null;
        RaiseProgress();
        return true;
    }

    /// <summary>Marks an item keep or remove. An item held by the identity provider cannot be removed here.</summary>
    public bool Decide(string itemId, ReviewDecision decision)
    {
        var item = _worklist.Find(i => i.Id == itemId);
        if (item is null)
        {
            Error = "That item is not in this review.";
            return false;
        }

        if (decision == ReviewDecision.Remove && item.Assignment.LockReason is { } reason)
        {
            Error = reason;
            return false;
        }

        Error = null;
        _decisions[itemId] = decision;
        RaiseProgress();
        return true;
    }

    /// <summary>Sends the decisions made so far. Items left undecided stay open.</summary>
    public async Task<bool> SaveDecisionsAsync()
    {
        if (CampaignId is null || _decisions.Count == 0)
        {
            return false;
        }

        var id = CampaignId;
        var sent = new Dictionary<string, ReviewDecision>(_decisions);
        var ok = await TryAsync(() => Gateway.SubmitDecisionsAsync(id, sent));
        if (!ok)
        {
            return false;
        }

        Status = $"{sent.Count} decision(s) saved.";
        await OpenAsync(id);
        await RefreshAsync();
        return true;
    }

    /// <summary>Closes the campaign. Refused while any item is undecided.</summary>
    public async Task<bool> CompleteAsync()
    {
        if (CampaignId is null)
        {
            return false;
        }

        if (Decided < Total)
        {
            Error = $"{Total - Decided} assignment(s) still need a decision.";
            return false;
        }

        if (_decisions.Count > 0 && !await SaveDecisionsAsync())
        {
            return false;
        }

        var id = CampaignId;
        var report = await CallAsync(() => Gateway.CompleteCampaignAsync(id));
        if (report is null)
        {
            return false;
        }

        Report = report;
        Status = "Review completed.";
        await RefreshAsync();
        return true;
    }

    private void RaiseProgress()
    {
        OnPropertyChanged(nameof(Worklist));
        OnPropertyChanged(nameof(Decided));
        OnPropertyChanged(nameof(Total));
        OnPropertyChanged(nameof(Progress));
    }
}
