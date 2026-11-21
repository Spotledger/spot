namespace Spotledger.Demo.Mock.Contracts;

/// <summary>Demo envelopes follow native API error, version and command-id conventions.</summary>
public sealed record FieldError(string Path, string Code, string Message);
public sealed record ApiError(string Code, string Message, IReadOnlyList<FieldError> FieldErrors);
public sealed record ApiResult<T>(T? Value, ApiError? Error, string? Version, string CommandId) where T : class
{
    public bool IsSuccess => Error is null;
}
public static class ApiResults
{
    public static ApiResult<T> Ok<T>(T value, string commandId, string? version = null) where T : class
        => new(value, null, version, commandId);
    public static ApiResult<T> Fail<T>(string commandId, string code, string message, params FieldError[] fields) where T : class
        => new(null, new(code, message, fields), null, commandId);
}
