using System.Runtime.InteropServices;
using System.Runtime.Versioning;

namespace Spotledger.Client.Theme;

/// <summary>
/// The Windows high-contrast read: <c>SystemParametersInfo(SPI_GETHIGHCONTRAST, ...)</c>.
/// The P/Invoke lives only in this file and only ever runs behind the
/// <see cref="OperatingSystem.IsWindows"/> check in <see cref="HighContrastDetector.Create"/>,
/// so a Linux or macOS process cannot load <c>user32.dll</c>.
///
/// Wiring this detector into the Desktop app (polling it, and applying
/// <see cref="HighContrastDetector.Resolve"/>) is a follow-up task: the Desktop project is
/// not owned by WP-UI-15.T3. See <c>clients/avalonia/docs/high-contrast.md</c>.
/// </summary>
[SupportedOSPlatform("windows")]
internal sealed class WindowsHighContrastDetector : IHighContrastDetector
{
    private const uint SpiGetHighContrast = 0x0042;
    private const uint SpiUpdateIniFile = 0x00000000;

    /// <inheritdoc />
    public HighContrastState Read()
    {
        var info = new HighContrastInfo { Size = (uint)Marshal.SizeOf<HighContrastInfo>() };
        if (!SystemParametersInfo(SpiGetHighContrast, info.Size, ref info, SpiUpdateIniFile))
        {
            // The call failed (a locked-down session, a very old shell). Fail open: an
            // exception on a theme read must never take the window down, and guessing
            // "high contrast" from nothing would repaint the whole application.
            return HighContrastState.Disabled;
        }

        return HighContrastDetector.FromHighContrastFlags(info.Flags);
    }

    /// <summary>The Win32 <c>HIGHCONTRAST</c> structure.</summary>
    [StructLayout(LayoutKind.Sequential)]
    private struct HighContrastInfo
    {
        public uint Size;
        public uint Flags;
        public IntPtr DefaultScheme;
    }

    [DllImport("user32.dll", EntryPoint = "SystemParametersInfoW", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SystemParametersInfo(uint action, uint parameter, ref HighContrastInfo info, uint winIni);
}
