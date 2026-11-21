using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// A user's assignments with where each came from. An assignment the identity provider manages (or
/// an emergency session holds) is read-only here: the screen says why and revoking is refused.
/// </summary>
public sealed class AssignmentsViewModel : IdentityViewModel
{
    private IReadOnlyList<AssignmentInfo> _items = [];

    /// <summary>Creates the screen.</summary>
    public AssignmentsViewModel(IIdentityGateway gateway, IIdentityShell shell)
        : base(gateway, shell)
    {
    }

    /// <summary>The user's assignments.</summary>
    public IReadOnlyList<AssignmentInfo> Items
    {
        get => _items;
        private set => SetField(ref _items, value);
    }

    /// <summary>The user whose assignments are loaded.</summary>
    public string? UserId { get; private set; }

    /// <summary>Loads a user's assignments.</summary>
    public async Task LoadAsync(string userId)
    {
        UserId = userId;
        var items = await CallAsync(() => Gateway.ListAssignmentsAsync(userId));
        if (items is not null)
        {
            Items = items;
        }
    }

    /// <summary>True when the assignment can be revoked on this screen.</summary>
    public static bool CanRevoke(AssignmentInfo assignment)
    {
        ArgumentNullException.ThrowIfNull(assignment);
        return assignment.LockReason is null;
    }

    /// <summary>Revokes an assignment after confirmation. A locked assignment is refused with its reason.</summary>
    public async Task<bool> RevokeAsync(AssignmentInfo assignment)
    {
        ArgumentNullException.ThrowIfNull(assignment);
        if (assignment.LockReason is { } reason)
        {
            Error = reason;
            return false;
        }

        var confirmed = await Shell.ConfirmAsync(
            "Revoke assignment",
            $"{assignment.RoleId} for {assignment.Scope.Describe()} ends now.",
            "Revoke");
        if (!confirmed)
        {
            return false;
        }

        var done = await TryAsync(() => Gateway.RevokeAssignmentAsync(assignment.Id));
        if (!done)
        {
            return false;
        }

        Status = "Assignment revoked.";
        if (UserId is not null)
        {
            await LoadAsync(UserId);
        }

        return true;
    }
}
