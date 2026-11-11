using System.Diagnostics;
using Xunit;

namespace Spotledger.Client.Browser.Shell.Tests;

/// <summary>
/// Runs the Node tests of the service worker: the pure policy file (wwwroot/sw/shell-policy.js) against the golden
/// vectors the C# tests use, and the worker itself (spot-sw.js) in a vm over a fake Cache API and network. When
/// node is not installed the test returns without checking: run <c>node Js/sw-policy.test.mjs</c> and
/// <c>node Js/sw-worker.test.mjs</c> from the test project folder by hand then (documented in the library README).
/// </summary>
public sealed class SwPolicyNodeTests
{
    [Theory]
    [InlineData("sw-policy.test.mjs")]
    [InlineData("sw-worker.test.mjs")]
    public void NodeTestPasses(string name)
    {
        string script = Path.Combine(AppContext.BaseDirectory, "Js", name);
        var info = new ProcessStartInfo("node", [script])
        {
            RedirectStandardOutput = true,
            RedirectStandardError = true,
        };
        Process? process;
        try
        {
            process = Process.Start(info);
        }
        catch (System.ComponentModel.Win32Exception)
        {
            return;
        }

        using (process)
        {
            Assert.NotNull(process);
            string output = process!.StandardOutput.ReadToEnd();
            string error = process.StandardError.ReadToEnd();
            process.WaitForExit();
            Assert.True(process.ExitCode == 0, error);
            Assert.Contains("ok", output);
        }
    }
}
