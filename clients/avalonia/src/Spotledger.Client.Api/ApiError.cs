using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Api;

/// <summary>
/// A per-field validation error from the contract's <c>error.fields</c>.
/// </summary>
/// <param name="Field">The field path, e.g. <c>items[2].qty</c>.</param>
/// <param name="Code">The dotted stable error code.</param>
/// <param name="Message">The short human-readable message.</param>
public sealed record ApiFieldError(string Field, string Code, string Message);

/// <summary>
/// The contract's error body (ADR-055 <c>ErrorEnvelope.error</c>) mapped into typed data
/// the shell can render: <see cref="Code"/>, <see cref="Message"/> and the per-field
/// validation errors. Never carries credentials or tokens.
/// </summary>
public sealed class ApiError
{
    /// <summary>HTTP status the error arrived with (0 for transport failures).</summary>
    public int StatusCode { get; init; }

    /// <summary>The stable dotted error code, e.g. <c>auth.invalid_credentials</c>.</summary>
    public required string Code { get; init; }

    /// <summary>Short human-readable message from the server.</summary>
    public required string Message { get; init; }

    /// <summary>Longer offline help text when the server sent <c>explanation</c>.</summary>
    public string? Explanation { get; init; }

    /// <summary>True when the contract marks the code retryable unchanged.</summary>
    public bool Retryable { get; init; }

    /// <summary>Server-side correlation id (<c>request_id</c>).</summary>
    public string? RequestId { get; init; }

    /// <summary>Per-field validation errors, empty when the server sent none.</summary>
    public IReadOnlyList<ApiFieldError> FieldErrors { get; init; } = Array.Empty<ApiFieldError>();

    /// <summary>Maps a Kiota-parsed <see cref="ErrorEnvelope"/> plus the HTTP status into an <see cref="ApiError"/>.</summary>
    public static ApiError FromEnvelope(ErrorEnvelope envelope, int statusCode)
    {
        ArgumentNullException.ThrowIfNull(envelope);
        var body = envelope.Error;
        var fields = new List<ApiFieldError>();
        if (body?.Fields is { } generatedFields)
        {
            foreach (var field in generatedFields)
            {
                if (field is null)
                {
                    continue;
                }

                fields.Add(new ApiFieldError(
                    field.Field ?? string.Empty,
                    field.Code ?? string.Empty,
                    field.Message ?? string.Empty));
            }
        }

        return new ApiError
        {
            StatusCode = statusCode,
            Code = body?.Code ?? "api.unknown_error",
            Message = body?.Message ?? "The server returned an error without a message.",
            Explanation = body?.Explanation,
            Retryable = body?.Retryable ?? false,
            RequestId = body?.RequestId,
            FieldErrors = fields,
        };
    }

    /// <summary>An error for a request that never got a contract-shaped response body (proxy errors, DNS failures).</summary>
    public static ApiError TransportFailure(int statusCode, string message) => new()
    {
        StatusCode = statusCode,
        Code = "api.unreachable",
        Message = message,
        Retryable = true,
    };
}

/// <summary>
/// Thrown by <see cref="ApiConnection"/> helpers when the server answers with the
/// contract's error envelope or the transport fails; carries the mapped <see cref="ApiError"/>.
/// </summary>
public sealed class ApiRequestException : Exception
{
    public ApiRequestException(ApiError error)
        : base(error?.Message ?? "The API request failed.")
    {
        Error = error ?? throw new ArgumentNullException(nameof(error));
    }

    public ApiRequestException(ApiError error, Exception innerException)
        : base(error?.Message ?? "The API request failed.", innerException)
    {
        Error = error ?? throw new ArgumentNullException(nameof(error));
    }

    /// <summary>The contract error behind this failure.</summary>
    public ApiError Error { get; }
}
