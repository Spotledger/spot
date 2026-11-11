namespace Spotledger.Client.Api;

/// <summary>
/// The OS credential store behind "Remember me" (WP-UI-02.T3): Windows Credential Manager,
/// Linux Secret Service via <c>secret-tool</c>, or the in-memory fallback. Only the
/// session token is stored under key <c>&lt;server&gt;/&lt;user&gt;</c> — never a password;
/// "Remember me" is off by default and opting in is the only path that writes here.
/// </summary>
public interface ISecretStore
{
    /// <summary>Returns the secret for <c>&lt;server&gt;/&lt;user&gt;</c>, or null when none is stored.</summary>
    Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default);

    /// <summary>Stores the secret (the session token) under <c>&lt;server&gt;/&lt;user&gt;</c>.</summary>
    Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default);

    /// <summary>Removes the secret; deleting an absent key is not an error.</summary>
    Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default);
}

/// <summary>Builds the <c>&lt;server&gt;/&lt;user&gt;</c> key shape every implementation shares.</summary>
public static class SecretKey
{
    /// <summary>Turns the picked server URL authority and the signed-in user into the key.</summary>
    public static string For(string serverUrl, string user)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(serverUrl);
        ArgumentException.ThrowIfNullOrWhiteSpace(user);
        string server;
        if (Uri.TryCreate(serverUrl, UriKind.Absolute, out var uri))
        {
            server = uri.GetLeftPart(UriPartial.Authority);
        }
        else
        {
            server = serverUrl.Trim().TrimEnd('/');
        }

        return server + "/" + user.Trim();
    }
}