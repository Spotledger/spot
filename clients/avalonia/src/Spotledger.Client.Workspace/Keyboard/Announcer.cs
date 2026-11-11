using Avalonia.Automation;
using Avalonia.Controls;

namespace Spotledger.Client.Workspace.Keyboard;

/// <summary>
/// The screen-reader live region of the workspace: a visible, assertive text block that the
/// arrangement writes one sentence into whenever something changed that a person who cannot see the
/// screen must hear (a guard appeared, a save failed and where the focus went, a row was selected).
/// </summary>
public sealed class Announcer : TextBlock
{
    /// <summary>The automation id of the region itself.</summary>
    public const string AutomationId = "split.live";

    /// <summary>Creates an empty region.</summary>
    public Announcer()
    {
        AutomationProperties.SetAutomationId(this, AutomationId);
        AutomationProperties.SetName(this, "Announcements");
        AutomationProperties.SetLiveSetting(this, AutomationLiveSetting.Assertive);
        TextWrapping = Avalonia.Media.TextWrapping.Wrap;
        Opacity = 0.75;
        Text = string.Empty;
    }

    /// <summary>Says one sentence; an empty sentence is ignored.</summary>
    public void Say(string text, FocusTrace? trace = null)
    {
        if (string.IsNullOrWhiteSpace(text))
        {
            return;
        }

        Text = text;
        trace?.Announce(text);
    }
}
