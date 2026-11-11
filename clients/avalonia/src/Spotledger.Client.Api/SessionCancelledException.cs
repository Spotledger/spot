namespace Spotledger.Client.Api;

/// <summary>
/// Thrown by <see cref="ApiConnection.RunSessionAwareAsync{T}"/> when the mid-session 401
/// prompt was cancelled (or re-signed-in and the retry failed again): the caller gets its
/// original error back in <see cref="Inner"/> and the shell shows the sign-in step with
/// the open tabs preserved read-only.
/// </summary>
public sealed class SessionCancelledException : Exception
{
    public SessionCancelledException(ApiError error, Exception? innerException = null)
        : base("The sign-in prompt was cancelled.", innerException)
    {
        Error = error ?? throw new ArgumentNullException(nameof(error));
    }

    /// <summary>The contract error behind the original 401.</summary>
    public ApiError Error { get; }
}