namespace Spotledger.Client.Api;

/// <summary>
/// One row of the signed-in user's <c>GET /api/v1/auth/sessions</c> answer (planned
/// contract v0.3.0, WP-CON-03.T4 — hand-written while the generated client does not carry
/// it; fields snake_case in the wire body via the connection's contract JSON options).
/// Nothing secret: only non-credential session metadata.
/// </summary>
/// <param name="Id">The session id (opaque token for DELETE /api/v1/auth/sessions/{id}).</param>
/// <param name="CreatedAt">When the session began (contract UTC stamp).</param>
/// <param name="LastUsedAt">The last request this session served (UTC stamp).</param>
/// <param name="UserAgent">The client the session was opened from, as the server saw it.</param>
/// <param name="Current">True for the session this app is using right now.</param>
public sealed record ClientSessionSummary(
    string Id,
    string? CreatedAt,
    string? LastUsedAt,
    string? UserAgent,
    bool Current);