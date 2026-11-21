using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// Emergency access: request with a reason and a bounded duration, approve or refuse, the banner
/// shown while a session is active, and the controller's review queue. A controller cannot review a
/// session they ran themselves.
/// </summary>
public sealed class EmergencyAccessViewModel : IdentityViewModel
{
    private IReadOnlyList<EmergencyGrantInfo> _mine = [];
    private IReadOnlyList<EmergencyGrantInfo> _toApprove = [];
    private IReadOnlyList<EmergencyReviewTask> _reviews = [];
    private EmergencyPolicyInfo _policy = new(60);

    /// <summary>Creates the screen for the signed-in user.</summary>
    public EmergencyAccessViewModel(IIdentityGateway gateway, IIdentityShell shell, string currentUserId)
        : base(gateway, shell)
    {
        CurrentUserId = currentUserId ?? throw new ArgumentNullException(nameof(currentUserId));
    }

    /// <summary>The signed-in user.</summary>
    public string CurrentUserId { get; }

    /// <summary>The caller's own grants.</summary>
    public IReadOnlyList<EmergencyGrantInfo> Mine
    {
        get => _mine;
        private set
        {
            if (SetField(ref _mine, value))
            {
                OnPropertyChanged(nameof(Active));
            }
        }
    }

    /// <summary>Requests waiting for this user's approval.</summary>
    public IReadOnlyList<EmergencyGrantInfo> ToApprove
    {
        get => _toApprove;
        private set => SetField(ref _toApprove, value);
    }

    /// <summary>The controller's review queue.</summary>
    public IReadOnlyList<EmergencyReviewTask> Reviews
    {
        get => _reviews;
        private set => SetField(ref _reviews, value);
    }

    /// <summary>The policy limits.</summary>
    public EmergencyPolicyInfo Policy
    {
        get => _policy;
        private set => SetField(ref _policy, value);
    }

    /// <summary>The caller's active session, if any.</summary>
    public EmergencyGrantInfo? Active => Mine.FirstOrDefault(g => g.Phase == EmergencyPhase.Active);

    /// <summary>
    /// The text of the banner every screen shows while a session is active, or null. The current
    /// time is an argument: nothing here reads the clock.
    /// </summary>
    public string? Banner(DateTimeOffset now)
    {
        if (Active is not { } session || session.EndsAt is not { } ends)
        {
            return null;
        }

        var left = ends - now;
        return left <= TimeSpan.Zero
            ? "Emergency access has ended."
            : $"Emergency access is on. Everything you do is recorded. {Math.Ceiling(left.TotalMinutes)} minute(s) left.";
    }

    /// <summary>Loads the policy, grants and review queue.</summary>
    public async Task RefreshAsync()
    {
        var policy = await CallAsync(() => Gateway.GetEmergencyPolicyAsync());
        if (policy is not null)
        {
            Policy = policy;
        }

        var lists = await CallAsync(() => Gateway.ListEmergencyAsync());
        if (lists is { } value)
        {
            Mine = value.Mine;
            ToApprove = value.ToApprove;
        }

        var reviews = await CallAsync(() => Gateway.ListEmergencyReviewsAsync());
        if (reviews is not null)
        {
            Reviews = reviews;
        }
    }

    /// <summary>Requests emergency access. A reason and a duration within the policy are required.</summary>
    public async Task<bool> RequestAsync(EmergencyRequestDraft draft)
    {
        ArgumentNullException.ThrowIfNull(draft);
        if (string.IsNullOrWhiteSpace(draft.Reason) || draft.Reason.Trim().Length < 10)
        {
            Error = "Say why you need emergency access (at least a short sentence).";
            return false;
        }

        if (draft.Minutes < 1 || draft.Minutes > Policy.MaxMinutes)
        {
            Error = $"Choose a duration between 1 and {Policy.MaxMinutes} minutes.";
            return false;
        }

        if (Mine.Any(g => g.Phase is EmergencyPhase.AwaitingApproval or EmergencyPhase.Active))
        {
            Error = "You already have an emergency request in progress.";
            return false;
        }

        var grant = await CallAsync(() => Gateway.RequestEmergencyAsync(draft));
        if (grant is null)
        {
            return false;
        }

        Status = "Request sent to the role owner.";
        await RefreshAsync();
        return true;
    }

    /// <summary>Approves or refuses a request. The requester cannot approve their own.</summary>
    public async Task<bool> DecideAsync(string grantId, bool approve, string? reason)
    {
        var grant = ToApprove.FirstOrDefault(g => g.Id == grantId);
        if (grant is null)
        {
            Error = "That request is not waiting for you.";
            return false;
        }

        if (grant.UserId == CurrentUserId)
        {
            Error = "You cannot approve your own emergency request.";
            return false;
        }

        if (!approve && string.IsNullOrWhiteSpace(reason))
        {
            Error = "Say why you are refusing it.";
            return false;
        }

        var ok = await TryAsync(() => Gateway.DecideEmergencyAsync(grantId, approve, reason?.Trim()));
        if (ok)
        {
            Status = approve ? "Approved." : "Refused.";
            await RefreshAsync();
        }

        return ok;
    }

    /// <summary>Ends the caller's active session early.</summary>
    public async Task<bool> EndAsync()
    {
        if (Active is not { } session)
        {
            return false;
        }

        var ok = await TryAsync(() => Gateway.EndEmergencyAsync(session.Id));
        if (ok)
        {
            Status = "Emergency access ended. A controller will review the session.";
            await RefreshAsync();
        }

        return ok;
    }

    /// <summary>Confirms a review. Refused for a session the controller ran themselves.</summary>
    public async Task<bool> ConfirmReviewAsync(string reviewId)
    {
        if (!MayReview(reviewId))
        {
            return false;
        }

        var ok = await TryAsync(() => Gateway.ConfirmEmergencyReviewAsync(reviewId));
        if (ok)
        {
            Status = "Review confirmed.";
            await RefreshAsync();
        }

        return ok;
    }

    /// <summary>Flags a session for a person to look at; a reason is required.</summary>
    public async Task<bool> FlagReviewAsync(string reviewId, string reason)
    {
        if (!MayReview(reviewId))
        {
            return false;
        }

        if (string.IsNullOrWhiteSpace(reason))
        {
            Error = "Say what looks wrong.";
            return false;
        }

        var ok = await TryAsync(() => Gateway.FlagEmergencyReviewAsync(reviewId, reason.Trim()));
        if (ok)
        {
            Status = "Session flagged.";
            await RefreshAsync();
        }

        return ok;
    }

    private bool MayReview(string reviewId)
    {
        var review = Reviews.FirstOrDefault(r => r.Id == reviewId);
        if (review is null)
        {
            Error = "That review is not in your queue.";
            return false;
        }

        if (review.SessionUserId == CurrentUserId)
        {
            Error = "You cannot review a session you ran yourself.";
            return false;
        }

        return true;
    }
}
