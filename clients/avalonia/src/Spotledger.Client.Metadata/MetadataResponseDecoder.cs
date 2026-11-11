using Microsoft.Kiota.Abstractions;
using Microsoft.Kiota.Abstractions.Serialization;
using Microsoft.Kiota.Serialization.Json;
using Spotledger.Client.Api.Generated.Api.V1.Meta.Item;
using Spotledger.Client.Api.Generated.Models;

namespace Spotledger.Client.Metadata;

// Override only the enum decoding boundary: Kiota's generated closed enum discards
// unknown wire text. All other metadata continues through its generated deserializers.
internal sealed class PreservedFieldHints : FieldHints
{
    internal string? RawControl { get; private set; }
    public override IDictionary<string, Action<IParseNode>> GetFieldDeserializers()
    {
        var fields = base.GetFieldDeserializers();
        fields["control"] = node => RawControl = node.GetStringValue();
        return fields;
    }
}

internal sealed class PreservedFieldMeta : FieldMeta
{
    internal IReadOnlyList<string>? MultipleDefault { get; private set; }

    public override IDictionary<string, Action<IParseNode>> GetFieldDeserializers()
    {
        var fields = base.GetFieldDeserializers();
        fields["hints"] = node => Hints = node.GetObjectValue(_ => new PreservedFieldHints());
        var decodeDefault = fields["default"];
        fields["default"] = node =>
        {
            var value = node.GetObjectValue(UntypedNode.CreateFromDiscriminatorValue);
            if (value is UntypedArray array)
            {
                MultipleDefault = Array.AsReadOnly(array.GetValue().OfType<UntypedString>().Select(item => item.GetValue()!).ToArray());
            }
            else
            {
                decodeDefault(node);
            }
        };
        return fields;
    }
}

internal sealed class PreservedDocTypeMeta : DocTypeMeta
{
    public override IDictionary<string, Action<IParseNode>> GetFieldDeserializers()
    {
        var fields = base.GetFieldDeserializers();
        fields["fields"] = node => Fields = node.GetCollectionOfObjectValues<FieldMeta>(_ => new PreservedFieldMeta())?.ToList();
        return fields;
    }
}

internal sealed class PreservedMetadataResponse : WithDoctypeGetResponse
{
    public override IDictionary<string, Action<IParseNode>> GetFieldDeserializers()
    {
        var fields = base.GetFieldDeserializers();
        fields["data"] = node => Data = node.GetObjectValue(_ => new PreservedDocTypeMeta());
        return fields;
    }
}

/// <summary>Lossless native metadata decoding, including future control-hint text.</summary>
public static class MetadataResponseDecoder
{
    /// <summary>Decodes a meta_get response without losing unknown control enum values.</summary>
    public static DocTypeModel Decode(string json)
    {
        using var document = System.Text.Json.JsonDocument.Parse(json);
        var node = new JsonParseNode(document.RootElement);
        var response = node.GetObjectValue(_ => new PreservedMetadataResponse());
        return MetaModelFactory.Create(response?.Data ?? throw new ArgumentException("Missing metadata data.", nameof(json)));
    }
}

internal sealed class MetadataResponseHandler : IResponseHandler
{
    public async Task<ModelType?> HandleResponseAsync<NativeResponseType, ModelType>(NativeResponseType response,
        Dictionary<string, ParsableFactory<IParsable>>? errorMappings = null)
    {
        if (response is not HttpResponseMessage message)
        {
            throw new InvalidOperationException("Expected an HTTP metadata response.");
        }
        using (message)
        {
            if (message.StatusCode == System.Net.HttpStatusCode.NotModified)
            {
                return default;
            }
            using var stream = await message.Content.ReadAsStreamAsync().ConfigureAwait(false);
            using var document = await System.Text.Json.JsonDocument.ParseAsync(stream).ConfigureAwait(false);
            var node = new JsonParseNode(document.RootElement);
            if (!message.IsSuccessStatusCode)
            {
                var error = node.GetObjectValue(ErrorEnvelope.CreateFromDiscriminatorValue)
                    ?? throw new InvalidOperationException("Missing metadata error envelope.");
                error.ResponseStatusCode = (int)message.StatusCode;
                throw error;
            }
            var decoded = node.GetObjectValue(_ => new PreservedMetadataResponse());
            return (ModelType?)(object?)decoded;
        }
    }
}
