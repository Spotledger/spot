using System.Reflection;
using System.Text;
using System.Text.Json;
using Microsoft.Kiota.Abstractions;
using Microsoft.Kiota.Abstractions.Serialization;
using Spotledger.Client.Api;
using Spotledger.Client.Api.Generated.Api.V1.Report.Item.Run;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Reports;

/// <summary>
/// The production <see cref="IReportGateway"/>: the generated <c>report</c> request builders through the
/// connection's session handling. The request is built by the generated client; the answer is read as the raw
/// JSON body and decoded by <see cref="ReportJson"/>. Two reasons: a decimal then keeps every digit it arrived
/// with (no generated model, no untyped tree in between), and the generated model of a parameter's
/// <c>options</c> (a <c>oneOf</c>) drops the link's DocType. The contract (0.7.0) has no export operation for
/// reports, so <see cref="CanExportOnServer"/> is false here.
/// </summary>
public sealed class NativeReportGateway(ApiConnection connection) : IReportGateway
{
    private static readonly PropertyInfo AdapterProperty = typeof(BaseRequestBuilder)
        .GetProperty("RequestAdapter", BindingFlags.Instance | BindingFlags.Public | BindingFlags.NonPublic)
        ?? throw new InvalidOperationException("The generated client no longer exposes its request adapter.");

    /// <inheritdoc />
    public async Task<IReadOnlyList<ReportSummary>> ListAsync(CancellationToken cancellationToken)
        => ReportJson.ReadSummaries(await SendAsync(connection.Client.Api.V1.Report.ToGetRequestInformation(), cancellationToken).ConfigureAwait(false));

    /// <inheritdoc />
    public async Task<ReportDescription> DescribeAsync(string report, CancellationToken cancellationToken)
        => ReportJson.ReadDescription(await SendAsync(connection.Client.Api.V1.Report[report].ToGetRequestInformation(), cancellationToken).ConfigureAwait(false));

    /// <inheritdoc />
    public async Task<ReportRunResult> RunAsync(string report, string filtersJson, int limit, CancellationToken cancellationToken)
    {
        var body = new RunPostRequestBody { Limit = limit, Filters = new RunPostRequestBody_filters() };
        using (var filters = JsonDocument.Parse(filtersJson))
        {
            foreach (var property in filters.RootElement.EnumerateObject())
            {
                body.Filters.AdditionalData[property.Name] = property.Value.ValueKind switch
                {
                    JsonValueKind.String => property.Value.GetString()!,
                    JsonValueKind.Number when property.Value.TryGetInt64(out var whole) => whole,
                    JsonValueKind.True => true,
                    JsonValueKind.False => false,
                    _ => throw new ArgumentException("A report filter is a text, a whole number or a flag.", nameof(filtersJson)),
                };
            }
        }

        return ReportJson.ReadRun(await SendAsync(connection.Client.Api.V1.Report[report].Run.ToPostRequestInformation(body), cancellationToken).ConfigureAwait(false));
    }

    /// <inheritdoc />
    public bool CanExportOnServer(ReportDescription description) => false;

    /// <inheritdoc />
    public Task ExportCsvAsync(ReportDescription description, string filtersJson, Stream destination, CancellationToken cancellationToken)
        => throw new NotSupportedException("The contract has no export operation for reports.");

    private async Task<string> SendAsync(RequestInformation request, CancellationToken cancellationToken)
    {
        var adapter = (IRequestAdapter)AdapterProperty.GetValue(connection.Client)!;
        var errors = new Dictionary<string, ParsableFactory<IParsable>>
        {
            ["XXX"] = ErrorEnvelope.CreateFromDiscriminatorValue,
        };
        Stream? stream;
        try
        {
            stream = await connection.RunSessionAwareAsync(() => adapter.SendPrimitiveAsync<Stream>(request, errors, cancellationToken), cancellationToken).ConfigureAwait(false);
        }
        catch (ErrorEnvelope envelope)
        {
            throw new ApiRequestException(ApiError.FromEnvelope(envelope, envelope.ResponseStatusCode));
        }

        if (stream is null)
        {
            throw new ApiRequestException(ApiError.TransportFailure(0, "The server answered without a body."));
        }

        using (stream)
        {
            using var reader = new StreamReader(stream, Encoding.UTF8);
            return await reader.ReadToEndAsync(cancellationToken).ConfigureAwait(false);
        }
    }
}
