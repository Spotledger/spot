using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Runtime.InteropServices.ComTypes;
using System.Runtime.Versioning;

namespace Spotledger.Client.Api;

/// <summary>
/// Windows Credential Manager behind "Remember me" (WP-UI-02.T3): P/Invoke to
/// <c>advapi32</c>'s <c>CredWriteW</c>/<c>CredReadW</c>/<c>CredDeleteW</c> — no extra
/// NuGet package. Only the session token is written, under the generic credential named
/// by <c>&lt;server&gt;/&lt;user&gt;</c>. Never called on a non-Windows host; the factory
/// decides (<see cref="SecretStores.CreateDefault"/>).
/// </summary>
[SupportedOSPlatform("windows")]
public sealed class WindowsCredentialSecretStore : ISecretStore
{
    private const int CredTypeGeneric = 1;
    private const int CredPersistLocalMachine = 2;
    private const int ErrorNotFound = 1168;

    public Task<string?> GetSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        if (!CredRead(key, CredTypeGeneric, 0, out var pointer))
        {
            var error = Marshal.GetLastWin32Error();
            return error == ErrorNotFound
                ? Task.FromResult<string?>(null)
                : throw new Win32Exception(error, $"The credential store refused to read '{key}'.");
        }

        try
        {
            var credential = Marshal.PtrToStructure<CREDENTIALW>(pointer);
            var size = credential.CredentialBlobSize;
            if (size <= 0 || credential.CredentialBlob == IntPtr.Zero)
            {
                return Task.FromResult<string?>(null);
            }

            var bytes = new byte[size];
            Marshal.Copy(credential.CredentialBlob, bytes, 0, size);
            return Task.FromResult<string?>(System.Text.Encoding.Unicode.GetString(bytes));
        }
        finally
        {
            CredFree(pointer);
        }
    }

    public Task SetSecretAsync(string key, string value, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        ArgumentException.ThrowIfNullOrWhiteSpace(value);
        var blob = System.Text.Encoding.Unicode.GetBytes(value);
        var buffer = Marshal.AllocHGlobal(blob.Length);
        try
        {
            Marshal.Copy(blob, 0, buffer, blob.Length);
            var credential = new CREDENTIALW
            {
                Flags = 0,
                Type = CredTypeGeneric,
                TargetName = key,
                Comment = "Spotledger session token",
                LastWritten = new FILETIME(),
                CredentialBlobSize = blob.Length,
                CredentialBlob = buffer,
                Persist = CredPersistLocalMachine,
                AttributeCount = 0,
                Attributes = IntPtr.Zero,
                TargetAlias = null,
                UserName = key[(key.LastIndexOf('/') + 1)..],
            };
            if (!CredWrite(ref credential, 0))
            {
                var error = Marshal.GetLastWin32Error();
                if (error == 0)
                {
                    throw new Win32Exception($"The credential store refused to write '{key}'.");
                }

                throw new Win32Exception(error, $"The credential store refused to write '{key}'.");
            }

            return Task.CompletedTask;
        }
        finally
        {
            Marshal.FreeHGlobal(buffer);
        }
    }

    public Task DeleteSecretAsync(string key, CancellationToken cancellationToken = default)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(key);
        if (!CredDelete(key, CredTypeGeneric, 0))
        {
            var error = Marshal.GetLastWin32Error();
            if (error is not (0 or ErrorNotFound))
            {
                throw new Win32Exception(error, $"The credential store refused to delete '{key}'.");
            }
        }

        return Task.CompletedTask;
    }

    /// <summary>Free the CREDENTIAL* that <c>CredReadW</c> allocated for the caller.</summary>
    [DllImport("advapi32.dll", SetLastError = false)]
    private static extern void CredFree(IntPtr credential);

    [DllImport("advapi32.dll", EntryPoint = "CredReadW", SetLastError = true, CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CredRead(
        [MarshalAs(UnmanagedType.LPWStr)] string targetName,
        int type,
        int reservedFlag,
        out IntPtr credential);

    [DllImport("advapi32.dll", EntryPoint = "CredWriteW", SetLastError = true, CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CredWrite(ref CREDENTIALW credential, int reservedFlags);

    [DllImport("advapi32.dll", EntryPoint = "CredDeleteW", SetLastError = true, CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CredDelete(
        [MarshalAs(UnmanagedType.LPWStr)] string targetName,
        int type,
        int reservedFlags);

    /// <summary>The contract's CREDENTIALW (wincred.h), only the generic-credential shape we use.</summary>
    [StructLayout(LayoutKind.Sequential, CharSet = CharSet.Unicode)]
    private struct CREDENTIALW
    {
        public int Flags;
        public int Type;
        [MarshalAs(UnmanagedType.LPWStr)] public string? TargetName;
        [MarshalAs(UnmanagedType.LPWStr)] public string? Comment;
        public FILETIME LastWritten;
        public int CredentialBlobSize;
        public IntPtr CredentialBlob;
        public int Persist;
        public int AttributeCount;
        public IntPtr Attributes;
        [MarshalAs(UnmanagedType.LPWStr)] public string? TargetAlias;
        [MarshalAs(UnmanagedType.LPWStr)] public string? UserName;
    }
}