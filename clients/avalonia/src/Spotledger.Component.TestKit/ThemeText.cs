using System.Reflection;
using Avalonia.Platform;

namespace Spotledger.Component.TestKit;

/// <summary>
/// Reads a component's style text for the review-time scan. A ControlTheme is compiled into the
/// package, so it is not always readable back as an asset; a package therefore also embeds the
/// same file as a manifest resource (one line in its project file), and the kit reads whichever
/// form the package ships. Without readable styles the token rule cannot be enforced at all,
/// which is why an unreadable theme is a contract violation rather than a skip.
/// </summary>
internal static class ThemeText
{
    /// <summary>The style text behind <paramref name="themeSource"/>, or null when nothing is readable.</summary>
    public static string? Read(Uri themeSource, out string failure)
    {
        failure = string.Empty;
        try
        {
            using var stream = AssetLoader.Open(themeSource);
            using var reader = new StreamReader(stream);
            return reader.ReadToEnd();
        }
        catch (Exception error) when (error is FileNotFoundException or ArgumentException or InvalidOperationException)
        {
            failure = error.Message;
        }

        var embedded = ReadEmbedded(themeSource, out var embeddedFailure);
        if (embedded is null)
        {
            failure = failure.Length == 0 ? embeddedFailure : failure + " " + embeddedFailure;
        }

        return embedded;
    }

    private static string? ReadEmbedded(Uri themeSource, out string failure)
    {
        failure = string.Empty;
        var fileName = Path.GetFileName(themeSource.AbsolutePath);
        Assembly assembly;
        try
        {
            assembly = Assembly.Load(new AssemblyName(themeSource.Host));
        }
        catch (Exception error) when (error is FileNotFoundException or FileLoadException or BadImageFormatException)
        {
            failure = "No assembly '" + themeSource.Host + "': " + error.Message;
            return null;
        }

        var match = assembly.GetManifestResourceNames()
            .FirstOrDefault(name => name.EndsWith(fileName, StringComparison.OrdinalIgnoreCase));
        if (match is null)
        {
            failure = "The package embeds no copy of '" + fileName + "' for review; add it as an EmbeddedResource.";
            return null;
        }

        using var stream = assembly.GetManifestResourceStream(match);
        if (stream is null)
        {
            failure = "'" + match + "' could not be opened.";
            return null;
        }

        using var reader = new StreamReader(stream);
        return reader.ReadToEnd();
    }
}
