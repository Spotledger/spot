namespace Spotledger.Client.Api;

/// <summary>
/// The features this build's generated client knows, keyed by the contract version that
/// added them (contracts/native-api/CHANGELOG.md). <see cref="ServerCapabilities"/> answers
/// per server: the one place the rest of the app asks "does this server have feature X" —
/// never scattered version checks.
/// </summary>
public static class ClientContractVersions
{
    /// <summary>The OpenAPI description the generated client was built from (the contract's info.version). Bump only with a regenerated client.</summary>
    public static readonly Version Current = new(0, 2, 0);

    /// <summary>Contract version that added each capability (CHANGELOG.md; 0.3.0 is planned in WP-CON-03.T4, not merged).</summary>
    public static readonly Version LookupSearchAdded = new(0, 2, 0);
    public static readonly Version RichFieldMetaAdded = new(0, 2, 0);
    public static readonly Version ExtendedFieldTypesAdded = new(0, 2, 0);
    public static readonly Version PasswordChangeAdded = new(0, 3, 0);
    public static readonly Version SessionManagementAdded = new(0, 3, 0);
}

/// <summary>
/// What the connected server supports. Built from <c>GET /api/health/live</c>'s
/// <c>version</c> against the client's contract version; a feature stays off unless the
/// server's version is at or past the version that added it.
/// </summary>
public sealed record ServerCapabilities
{
    /// <summary>True when the server is too new or too old to be usable (major mismatch).</summary>
    public required bool Compatible { get; init; }

    /// <summary>The version string the server reported, "unknown" when it sent none.</summary>
    public required string ServerVersion { get; init; }

    /// <summary>True when the server did not send a version (a contract-0.2 peer without the field).</summary>
    public bool VersionReported => !string.Equals(ServerVersion, "unknown", StringComparison.Ordinal);

    /// <summary>Non-blocking text when the server is ahead of this app; null when not.</summary>
    public required string? Notice { get; init; }

    public bool LookupSearch { get; init; }
    public bool RichFieldMeta { get; init; }
    public bool ExtendedFieldTypes { get; init; }
    public bool PasswordChange { get; init; }
    public bool SessionManagement { get; init; }

    /// <summary>The capabilities of a server never seen — every feature the client itself carries, on; nothing from a later contract.</summary>
    public static ServerCapabilities AssumedCurrent() => From(null);

    /// <summary>
    /// Builds capabilities from the server's reported version (null = not reported),
    /// which the shell reads as "this peer matches this client's contract".
    /// </summary>
    /// <exception cref="ApiVersionMismatchException">The server's major differs from the client's.</exception>
    public static ServerCapabilities From(Version? serverVersion)
    {
        if (serverVersion is null)
        {
            // Contract 0.2.0 peers do not send `version` (the field arrives with 0.3.0):
            // treat them as the current contract and keep everything this build knows on.
            return new ServerCapabilities
            {
                Compatible = true,
                ServerVersion = "unknown",
                Notice = null,
                LookupSearch = ClientContractVersions.Current >= ClientContractVersions.LookupSearchAdded,
                RichFieldMeta = ClientContractVersions.Current >= ClientContractVersions.RichFieldMetaAdded,
                ExtendedFieldTypes = ClientContractVersions.Current >= ClientContractVersions.ExtendedFieldTypesAdded,
                PasswordChange = false,
                SessionManagement = false,
            };
        }

        if (serverVersion.Major != ClientContractVersions.Current.Major)
        {
            throw new ApiVersionMismatchException(serverVersion, ClientContractVersions.Current);
        }

        var notice = serverVersion > ClientContractVersions.Current
            ? $"The server runs a newer API (version {Format(serverVersion)}); this app targets version {Format(ClientContractVersions.Current)}."
            : null;
        return new ServerCapabilities
        {
            Compatible = true,
            ServerVersion = Format(serverVersion),
            Notice = notice,
            LookupSearch = serverVersion >= ClientContractVersions.LookupSearchAdded,
            RichFieldMeta = serverVersion >= ClientContractVersions.RichFieldMetaAdded,
            ExtendedFieldTypes = serverVersion >= ClientContractVersions.ExtendedFieldTypesAdded,
            PasswordChange = serverVersion >= ClientContractVersions.PasswordChangeAdded,
            SessionManagement = serverVersion >= ClientContractVersions.SessionManagementAdded,
        };
    }

    private static string Format(Version version) =>
        version.Build < 0 ? $"{version.Major}.{version.Minor}" : version.ToString();
}

/// <summary>
/// The negotiation answer when the server speaks a different API major than the generated
/// client: <see cref="ServerVersion.Major"/> vs <see cref="ClientMajor"/>.
/// </summary>
public sealed class ApiVersionMismatchException(Version serverVersion, Version clientVersion)
    : Exception($"This server speaks API version {ServerMajor(serverVersion)}; this app needs {clientVersion.Major}.")
{
    public Version ServerVersion { get; } = serverVersion;

    public Version ClientVersion { get; } = clientVersion;

    private static int ServerMajor(Version version) => version.Major;
}