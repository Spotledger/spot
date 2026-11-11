using System.Security.Cryptography;

namespace Spotledger.Client.Analytics;

/// <summary>
/// The install-random id: 128 random bits as 32 lowercase hex characters, generated once per
/// install by the host and stored with the user settings. It is not derived from the user, the
/// machine or the server, so it cannot identify anyone; deleting it starts a new identity.
/// </summary>
public static class InstallId
{
    public static string Create() => Convert.ToHexStringLower(RandomNumberGenerator.GetBytes(16));

    public static bool IsValid(string? value)
    {
        if (value is null || value.Length != 32)
        {
            return false;
        }

        foreach (var c in value)
        {
            if (c is not (>= '0' and <= '9' or >= 'a' and <= 'f'))
            {
                return false;
            }
        }

        return true;
    }
}
