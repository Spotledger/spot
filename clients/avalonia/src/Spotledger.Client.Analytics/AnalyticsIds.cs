namespace Spotledger.Client.Analytics;

/// <summary>
/// The only things analytics may carry are short identifiers chosen by developers: screen, action,
/// shortcut and error-code ids. This is the gate. Anything that could hold user text (spaces,
/// '@', quotes, path separators, long strings) is refused, so a bug that passes a field value
/// as an id drops the event instead of leaking the value.
/// </summary>
public static class AnalyticsIds
{
    public const int MaxLength = 64;

    /// <summary>Screen, action and shortcut ids: lowercase letters, digits, '.', '_' and '-'.</summary>
    public static bool IsValidId(string? value) => IsValid(value, allowUpper: false);

    /// <summary>Error codes: the same shape, upper case allowed (for example <c>SP-1042</c>).</summary>
    public static bool IsValidCode(string? value) => IsValid(value, allowUpper: true);

    private static bool IsValid(string? value, bool allowUpper)
    {
        if (string.IsNullOrEmpty(value) || value.Length > MaxLength)
        {
            return false;
        }

        for (var i = 0; i < value.Length; i++)
        {
            var c = value[i];
            var ok = c is >= 'a' and <= 'z' or >= '0' and <= '9'
                || (allowUpper && c is >= 'A' and <= 'Z')
                || (i > 0 && c is '.' or '_' or '-');
            if (!ok)
            {
                return false;
            }
        }

        return true;
    }
}
