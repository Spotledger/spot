using System.Globalization;
using System.Net;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// A test double for <see cref="ApiConnection"/>'s MetaCache fetches: an
/// <see cref="HttpMessageHandler"/> that pretends to be the native API's meta endpoint,
/// honouring <c>If-None-Match</c>, answering 200 (+ ETag + body) or 304, recording the
/// request history for assertions. The served body is written with
/// <see cref="Utf8JsonWriter"/> so no string formatting is culture-sensitive.
/// </summary>
public sealed class FakeMetaHttpHandler : HttpMessageHandler
{
    private long _requestCount;

    /// <summary>How many requests reached this handler (any status).</summary>
    public long Requests => Interlocked.Read(ref _requestCount);

    /// <summary>The If-None-Match header of the last request; null when it carried none.</summary>
    public string? LastIfNoneMatch { get; private set; }

    /// <summary>The metadata version the fake server answers with; bump it to simulate
    /// a metadata change on the site.</summary>
    public int MetaVersion { get; set; } = 7;

    /// <summary>The version the fake server still considers current: when the request's
    /// If-None-Match names it, a 304 is answered.</summary>
    public int CurrentVersion { get; set; } = 7;

    /// <summary>The doctype the fake server answers with.</summary>
    public string Doctype { get; set; } = "sales_invoice";

    /// <summary>The DocType label the served body carries.</summary>
    public string Label { get; set; } = "Sales Invoice";

    /// <summary>Extra fields appended after the shared sample fields; tests shape the
    /// served metadata through them.</summary>
    public List<FieldMeta> ExtraFields { get; } = new();

    /// <summary>When true the served body embeds <see cref="CurrentVersion"/> instead of
    /// <see cref="MetaVersion"/> (200 body and 304 etag disagree: the revalidation
    /// guard's case).</summary>
    public bool ServeStaleBody { get; set; }

    /// <summary>When set, the fake server answers with a contract error envelope of
    /// this shape instead of metadata (any <c>4XX</c>/<c>5XX</c> surfaces to Kiota's
    /// error mapping).</summary>
    public (int StatusCode, string Code, string Message)? ServeError { get; set; }

    /// <summary>How many more requests the fake server answers by dropping the
    /// connection (a transport failure); the count decrements per dropped request.</summary>
    public int RemainingTransportFailures { get; set; }

    /// <inheritdoc/>
    protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
    {
        Interlocked.Increment(ref _requestCount);
        LastIfNoneMatch = request.Headers.TryGetValues("If-None-Match", out var values)
            ? string.Join(",", values)
            : null;

        if (RemainingTransportFailures > 0)
        {
            RemainingTransportFailures--;
            throw new HttpRequestException("The connection was reset (fake transport failure).");
        }

        if (ServeError is { } fail)
        {
            return Task.FromResult(new HttpResponseMessage((HttpStatusCode)fail.StatusCode)
            {
                Content = new StringContent(BuildErrorBody(fail.Code, fail.Message), Encoding.UTF8, "application/json"),
            });
        }

        var currentTag = CurrentVersion.ToString(CultureInfo.InvariantCulture);
        if (LastIfNoneMatch is { } inm && inm.Contains(currentTag, StringComparison.Ordinal))
        {
            return Task.FromResult(new HttpResponseMessage(HttpStatusCode.NotModified));
        }

        var body = BuildBody();
        var response = new HttpResponseMessage(HttpStatusCode.OK)
        {
            Content = new StringContent(body, Encoding.UTF8, "application/json"),
        };
        response.Headers.ETag = new EntityTagHeaderValue($"\"{MetaVersion.ToString(CultureInfo.InvariantCulture)}\"");
        return Task.FromResult(response);
    }

    private static string BuildErrorBody(string code, string message)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteStartObject("error");
            writer.WriteString("code", code);
            writer.WriteString("message", message);
            writer.WriteEndObject();
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private string BuildBody()
    {
        var fields = new List<FieldMeta>
        {
            new()
            {
                Name = "customer",
                Label = "Customer",
                Type = FieldType.Link,
                Required = true,
                LinkTo = "customer",
            },
            new()
            {
                Name = "grand_total",
                Label = "Grand Total",
                Type = FieldType.Decimal,
                ReadOnly = true,
                Scale = 2,
                Precision = 18,
                Default = new Spotledger.Client.Api.Generated.Models.FieldValue { String = "0.00" },
            },
        };
        fields.AddRange(ExtraFields);

        var version = ServeStaleBody ? CurrentVersion : MetaVersion;
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteStartObject("data");
            writer.WriteString("doctype", Doctype);
            writer.WriteString("label", Label);
            writer.WriteNumber("version", version);
            writer.WriteBoolean("is_posted", true);
            writer.WriteStartArray("fields");
            foreach (var field in fields)
            {
                WriteField(writer, field);
            }

            writer.WriteEndArray();
            writer.WriteEndObject();
            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private static void WriteField(Utf8JsonWriter writer, FieldMeta field)
    {
        writer.WriteStartObject();
        writer.WriteString("name", field.Name);
        writer.WriteString("label", field.Label);
        writer.WriteString("type", TypeNames[field.Type ?? FieldType.Text]);
        if (field.Required == true)
        {
            writer.WriteBoolean("required", true);
        }

        if (field.ReadOnly == true)
        {
            writer.WriteBoolean("read_only", true);
        }

        if (field.Scale is { } scale)
        {
            writer.WriteNumber("scale", scale);
        }

        if (field.Precision is { } precision)
        {
            writer.WriteNumber("precision", precision);
        }

        if (field.LinkTo is { } linkTo)
        {
            writer.WriteString("link_to", linkTo);
        }

        if (field.Default is { } d)
        {
            writer.WritePropertyName("default");
            WriteScalar(writer, d);
        }

        if (field.Select is { Count: > 0 } select)
        {
            writer.WriteStartArray("select");
            foreach (var option in select)
            {
                writer.WriteStringValue(option);
            }

            writer.WriteEndArray();
        }

        writer.WriteEndObject();
    }

    /// <summary>The contract's field-type wire names, for the served body.</summary>
    internal static readonly IReadOnlyDictionary<FieldType, string> TypeNames =
        new Dictionary<FieldType, string>
        {
            [FieldType.Bool] = "bool",
            [FieldType.Int] = "int",
            [FieldType.Decimal] = "decimal",
            [FieldType.Text] = "text",
            [FieldType.Long_text] = "long_text",
            [FieldType.Date] = "date",
            [FieldType.Time] = "time",
            [FieldType.Datetime] = "datetime",
            [FieldType.Link] = "link",
            [FieldType.Json] = "json",
            [FieldType.Bytes] = "bytes",
            [FieldType.Attach] = "attach",
            [FieldType.Secret] = "secret",
            [FieldType.Table] = "table",
            [FieldType.Table_multiselect] = "table_multiselect",
        };

    private static void WriteScalar(Utf8JsonWriter writer, Spotledger.Client.Api.Generated.Models.FieldValue value)
    {
        if (value.Boolean is { } b)
        {
            writer.WriteBooleanValue(b);
        }
        else if (value.Integer is { } i)
        {
            writer.WriteNumberValue(i);
        }
        else
        {
            writer.WriteStringValue(value.String);
        }
    }
}