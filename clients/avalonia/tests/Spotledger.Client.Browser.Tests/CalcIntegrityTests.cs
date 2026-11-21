using System.Diagnostics;
using Xunit;

namespace Spotledger.Client.Browser.Tests;

/// <summary>calc.mjs refuses a calculation module whose SHA-256 differs from assets.json (fail closed).
/// The check is JavaScript, so it runs under node; the test is skipped (returns) when node is absent.</summary>
public sealed class CalcIntegrityTests
{
    [Fact]
    public void CalcModuleIsCheckedAgainstTheManifest()
    {
        var script = Path.Combine(AppContext.BaseDirectory, "Js", "calc-integrity.mjs");
        var info = new ProcessStartInfo("node", ["--input-type=module", "-e",
            $"await import({System.Text.Json.JsonSerializer.Serialize(new Uri(script).AbsoluteUri)})"])
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        Process? process;
        try { process = Process.Start(info); }
        catch (System.ComponentModel.Win32Exception) { return; }
        using (process)
        {
            Assert.NotNull(process);
            var output = process!.StandardOutput.ReadToEnd();
            var error = process.StandardError.ReadToEnd();
            process.WaitForExit();
            Assert.True(process.ExitCode == 0, error);
            Assert.Contains("ok", output);
        }
    }
}
