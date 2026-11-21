namespace Spotledger.Client.Shell;

/// <summary>The three steps the app walks through: pick a server, sign in, use the shell.</summary>
public enum AppStage
{
    ServerPicker,
    Login,
    Shell,
}
