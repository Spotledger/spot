using System.Diagnostics;
using System.Runtime.InteropServices;

namespace Spotledger.Client.Grid.Tests;

/// <summary>
/// CPU time consumed by the calling thread, in milliseconds. A busy host steals wall time
/// from a test but not the thread's own CPU time, so a budget judged on this figure does
/// not depend on how loaded the runner is. Linux and macOS read
/// clock_gettime(CLOCK_THREAD_CPUTIME_ID), Windows reads GetThreadTimes; anywhere else the
/// process-wide CPU time is the fallback and <see cref="Source"/> says so.
/// </summary>
internal static class ThreadCpuTime
{
    private const int ClockThreadCpuTimeIdLinux = 3;
    private const int ClockThreadCpuTimeIdMac = 16;

    [StructLayout(LayoutKind.Sequential)]
    private struct Timespec
    {
        public long Seconds;
        public long Nanoseconds;
    }

    [DllImport("libc", EntryPoint = "clock_gettime")]
    private static extern int ClockGetTime(int clockId, out Timespec time);

    [DllImport("kernel32.dll", EntryPoint = "GetCurrentThread")]
    private static extern IntPtr GetCurrentThread();

    [DllImport("kernel32.dll", EntryPoint = "GetThreadTimes")]
    private static extern bool GetThreadTimes(IntPtr thread, out long creation, out long exit, out long kernel, out long user);

    private static bool _nativeFailed;

    /// <summary>What the last figure was read from, for failure messages.</summary>
    public static string Source { get; private set; } = "thread CPU time";

    /// <summary>The calling thread's CPU time in milliseconds.</summary>
    public static double NowMs()
    {
        if (!_nativeFailed)
        {
            try
            {
                if (OperatingSystem.IsWindows())
                {
                    if (GetThreadTimes(GetCurrentThread(), out _, out _, out var kernel, out var user))
                    {
                        Source = "thread CPU time";
                        // 100 ns units.
                        return (kernel + user) * 100 / 1_000_000d;
                    }
                }
                else if (OperatingSystem.IsLinux() || OperatingSystem.IsMacOS())
                {
                    var id = OperatingSystem.IsMacOS() ? ClockThreadCpuTimeIdMac : ClockThreadCpuTimeIdLinux;
                    if (ClockGetTime(id, out var ts) == 0)
                    {
                        Source = "thread CPU time";
                        var nanoseconds = ts.Seconds * 1_000_000_000L + ts.Nanoseconds;
                        return nanoseconds / 1_000_000d;
                    }
                }
            }
            catch (Exception e) when (e is DllNotFoundException or EntryPointNotFoundException)
            {
                // fall through to the process-wide figure
            }

            _nativeFailed = true;
        }

        Source = "process CPU time (thread CPU time is unavailable on this platform)";
        return Process.GetCurrentProcess().TotalProcessorTime.TotalMilliseconds;
    }
}
