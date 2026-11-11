using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated;
using Spotledger.Client.Api.Generated.Models;
using Xunit;

namespace Spotledger.Client.Shell.Tests;

/// <summary>
/// The build generated the contract client and it is reachable from the solution:
/// <see cref="NativeApiClient"/> exists and its auth/login request builder is navigable
/// from the root (contracts/native-api/openapi.yaml via Kiota at build time).
/// </summary>
public sealed class GeneratedClientTests
{
    private const string BaseUrl = "http://localhost:8099/";

    [Fact]
    public void NativeApiClientTypeExists()
    {
        var type = typeof(NativeApiClient);

        Assert.Equal("Spotledger.Client.Api.Generated", type.Namespace);
        Assert.Equal("NativeApiClient", type.Name);
    }

    [Fact]
    public void ApiV1AuthLoginRequestBuilderIsReachableFromTheRoot()
    {
        using var connection = new ApiConnection(new FakeServerHandler());
        connection.Connect(new Uri(BaseUrl));

        var login = connection.Client.Api.V1.Auth.Login;

        Assert.NotNull(login);

        // The request builder carries the contract's login path; the protected
        // UrlTemplate is what Kiota expands {+baseurl} against at execution time.
        var urlTemplate = typeof(Spotledger.Client.Api.Generated.Api.V1.Auth.Login.LoginRequestBuilder)
            .BaseType!
            .GetProperty("UrlTemplate", System.Reflection.BindingFlags.NonPublic | System.Reflection.BindingFlags.Instance)!
            .GetValue(login) as string;
        Assert.Equal("{+baseurl}/api/v1/auth/login", urlTemplate);
    }

    [Fact]
    public void ApiErrorMapsTheContractEnvelope()
    {
        var envelope = new ErrorEnvelope
        {
            Error = new ErrorEnvelope_error
            {
                Code = "validation.failed",
                Message = "Some fields need attention.",
                Retryable = false,
                RequestId = "req-1",
                Fields =
                [
                    new FieldError { Field = "qty", Code = "validation.min_value", Message = "qty is too small." },
                ],
            },
        };

        var error = ApiError.FromEnvelope(envelope, statusCode: 400);

        Assert.Equal(400, error.StatusCode);
        Assert.Equal("validation.failed", error.Code);
        Assert.Equal("Some fields need attention.", error.Message);
        Assert.Equal("req-1", error.RequestId);
        var field = Assert.Single(error.FieldErrors);
        Assert.Equal("qty", field.Field);
        Assert.Equal("validation.min_value", field.Code);
        Assert.Equal("qty is too small.", field.Message);
    }
}
