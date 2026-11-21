namespace Spotledger.Designer.Reports;

/// <summary>
/// Keeps a failed server call from escaping a command or a view handler: the person sees a plain
/// sentence in the status line instead of a silent failure or a crash.
/// </summary>
internal static class Guard
{
    /// <summary>The sentence shown when a call to the server failed.</summary>
    public static string Describe(Exception exception)
    {
        ArgumentNullException.ThrowIfNull(exception);
        return $"That did not work: the server could not be reached or did not answer ({exception.GetType().Name}). Nothing was changed here; try again.";
    }

    /// <summary>Runs the work; an exception (other than a cancellation) is reported through <paramref name="fail"/> and becomes false.</summary>
    public static async Task<bool> RunAsync(Func<Task<bool>> work, Action<string> fail)
    {
        ArgumentNullException.ThrowIfNull(work);
        ArgumentNullException.ThrowIfNull(fail);
        try
        {
            return await work().ConfigureAwait(true);
        }
        catch (OperationCanceledException)
        {
            throw;
        }
        catch (Exception exception)
        {
            fail(Describe(exception));
            return false;
        }
    }
}
