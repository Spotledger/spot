using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

/// <summary>
/// Deliverable 8's "every field kind round trip (parse, format, patch)": for every kind
/// the contract names, typed text parses into the kind's typed value, the value formats
/// back to the same contract text, and a draft edit surfaces as exactly that encoding in
/// the merge-patch body — plus the decimal law (scale violations are errors, never
/// rounding), the range cases (large, negative, 29 digits) and the unknown kind.
/// </summary>
public sealed class FieldValueRoundTripTests
{
    [Fact]
    public void TextParsesFormatsAndPatches()
    {
        var field = new FieldModel("message", "Message", FieldKind.Text);
        Assert.True(field.TryParse(" hello ", out var value, out var error));
        Assert.Null(error);
        var text = Assert.IsType<TextFieldValue>(value);
        Assert.Equal(" hello ", text.Value);
        Assert.Equal("string", ValueCodec.EncodingOf(text));

        var draft = DocumentOf(field);
        draft.Set("message", text);
        Assert.Equal("""{"message":" hello "}""", draft.ToPatch());
    }

    [Fact]
    public void LongTextRoundTripsLikeText()
    {
        var field = new FieldModel("terms", "Terms", FieldKind.LongText);
        Assert.True(field.TryParse("line one\nline two", out var value, out _));
        Assert.Equal("line one\nline two", Assert.IsType<TextFieldValue>(value).Value);
    }

    [Fact]
    public void IntegerParsesFormatsAndPatchesAsJsonNumber()
    {
        var field = new FieldModel("opening_number", "Opening", FieldKind.IntegerNumber);
        Assert.True(field.TryParse("42", out var value, out var error));
        Assert.Null(error);
        Assert.Equal(42L, Assert.IsType<IntFieldValue>(value).Value);
        Assert.True(field.TryParse("-9223372036854775807", out var negative, out _));
        Assert.Equal(-9223372036854775807L, Assert.IsType<IntFieldValue>(negative).Value);

        var draft = DocumentOf(field);
        draft.Set("opening_number", value!);
        Assert.Equal("""{"opening_number":42}""", draft.ToPatch());
    }

    [Fact]
    public void IntegerBeyondSixtyFourBitsIsOutOfRangeNotRounded()
    {
        var field = new FieldModel("opening_number", "Opening", FieldKind.IntegerNumber);
        Assert.False(field.TryParse("9223372036854775808", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.int_out_of_range", error!.Code);
    }

    [Theory]
    [InlineData("1250", "1250", false)]
    [InlineData("1250.00", "1250.00", false)]
    [InlineData("-12.50", "-12.50", true)]
    [InlineData("0.01", "0.01", false)]
    public void DecimalParsesFormatsAndPatchesAsStringAndNeverUsesFloatingPoint(
        string typed,
        string expected,
        bool negative)
    {
        var field = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.True(field.TryParse(typed, out var value, out var error));
        Assert.Null(error);
        var decimalValue = Assert.IsType<DecimalFieldValue>(value);
        Assert.Equal(expected, decimalValue.CanonicalText);
        Assert.Equal(negative, decimalValue.CanonicalText.StartsWith('-'));

        var draft = DocumentOf(field);
        draft.Set("rate", decimalValue);
        Assert.Equal($$"""{"rate":"{{expected}}"}""", draft.ToPatch());
    }

    [Fact]
    public void DecimalWithMorePlacesThanScaleIsAnErrorNeverRounded()
    {
        var field = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.False(field.TryParse("1250.005", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.scale_exceeded", error!.Code);
        Assert.Contains("no rounding", error.Message);

        Assert.False(field.TryParse("1250.999", out _, out error));
        Assert.Equal("field.scale_exceeded", error!.Code);
    }

    [Fact]
    public void DecimalBeyondTheFieldsPrecisionIsRejected()
    {
        var field = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.False(field.TryParse("12345678901234567.00", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.precision_exceeded", error!.Code);
    }

    [Fact]
    public void DecimalExponentAndGarbageAreRejected()
    {
        var field = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.False(field.TryParse("1e3", out _, out var error));
        Assert.Equal("field.invalid_decimal", error!.Code);
        Assert.False(field.TryParse("12,50", out _, out error, NumberParsingSettings.Strict));
        Assert.Equal("field.invalid_decimal", error!.Code);
    }

    [Fact]
    public void DecimalSeparatorsOnlyWithTheExplicitSetting()
    {
        var field = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.False(field.TryParse("1,250.50", out _, out _, NumberParsingSettings.Strict));
        Assert.True(field.TryParse("1,250.50", out var value, out _, NumberParsingSettings.WithSeparators));
        Assert.Equal("1250.50", Assert.IsType<DecimalFieldValue>(value).CanonicalText);
    }

    [Fact]
    public void DecimalFittingNativePrecisionIsDecimalElseStringBacked()
    {
        var inside = new FieldModel("rate", "Rate", FieldKind.DecimalNumber, scale: 2, precision: 18);
        Assert.True(inside.TryParse("9999999999999999.99", out var native, out var error));
        Assert.Null(error);
        var nativeValue = Assert.IsType<DecimalFieldValue>(native);
        Assert.True(nativeValue.IsNative, "18 of 28 significant digits stay a System.Decimal.");
        Assert.IsType<decimal>(nativeValue.ExactValue.AsDecimal);

        // The brief's 29-digit case: precision 29 does not fit decimal's 28 significant
        // digits, so the value is string-backed — and still exact.
        var beyond = new FieldModel("big", "Big", FieldKind.DecimalNumber, scale: 0, precision: 29);
        Assert.True(beyond.TryParse("12345678901234567890123456789", out var exact, out error));
        Assert.Null(error);
        // The brief's 29-digit case: precision 29 does not fit decimal's 28 significant
        // digits, so the value is string-backed — and still exact: all 29 digits
        // survive, no rounding, no float.
        var bigValue = Assert.IsType<DecimalFieldValue>(exact);
        Assert.False(bigValue.IsNative);
        Assert.Equal("12345678901234567890123456789", bigValue.CanonicalText);
        Assert.Throws<InvalidOperationException>(() => bigValue.ExactValue.AsDecimal);
    }

    [Fact]
    public void NegativeTwentyNineDigitDecimalIsExactToo()
    {
        var beyond = new FieldModel("big", "Big", FieldKind.DecimalNumber, scale: 2, precision: 31);
        Assert.True(beyond.TryParse("-12345678901234567890123456789.12", out var exact, out var error));
        Assert.Null(error);
        var bigValue = Assert.IsType<DecimalFieldValue>(exact);
        Assert.False(bigValue.IsNative);
        Assert.Equal("-12345678901234567890123456789.12", bigValue.CanonicalText);

        // The explicit rule, demonstrated: a System.Decimal cannot hold this value
        // exactly (29+ significant digits exceed the 96-bit mantissa) and silently
        // rounds the .12 away — which is exactly why the runtime keeps it string-backed.
        var lossy = decimal.Parse("-12345678901234567890123456789.12", CultureInfo.InvariantCulture);
        Assert.NotEqual("-12345678901234567890123456789.12", lossy.ToString(CultureInfo.InvariantCulture));

        // And the same value arriving from a document decodes equal to the parsed one.
        var loaded = ValueCodec.DecodeElement(
            JsonDocument.Parse("\"-12345678901234567890123456789.12\"").RootElement,
            FieldKind.DecimalNumber);
        Assert.Equal(bigValue, loaded);
    }

    [Fact]
    public void DateParsesFormatsAndPatches()
    {
        var field = new FieldModel("transaction_date", "Date", FieldKind.Date);
        Assert.True(field.TryParse("2026-09-12", out var value, out var error));
        Assert.Null(error);
        var date = Assert.IsType<DateFieldValue>(value);
        Assert.Equal(new DateOnly(2026, 9, 12), date.Value);
        Assert.False(field.TryParse("2026-13-40", out _, out error));
        Assert.Equal("field.invalid_date", error!.Code);

        var draft = DocumentOf(field);
        draft.Set("transaction_date", date);
        Assert.Equal("""{"transaction_date":"2026-09-12"}""", draft.ToPatch());
    }

    [Fact]
    public void TimeParsesFormatsAndPatches()
    {
        var field = new FieldModel("opened", "Opened", FieldKind.Time);
        Assert.True(field.TryParse("14:30:00", out var value, out var error));
        Assert.Null(error);
        Assert.Equal(new TimeOnly(14, 30), Assert.IsType<TimeFieldValue>(value).Value);
        Assert.False(field.TryParse("14:30", out _, out error));
        Assert.Equal("field.invalid_time", error!.Code);
    }

    [Fact]
    public void DatetimeParsesFormatsAndPatchesInUtc()
    {
        var field = new FieldModel("created_at", "Created", FieldKind.Datetime);
        Assert.True(field.TryParse("2026-09-12T16:30:00+02:00", out var value, out var error));
        Assert.Null(error);
        var moment = Assert.IsType<DateTimeFieldValue>(value);
        Assert.Equal(new DateTimeOffset(2026, 9, 12, 16, 30, 0, TimeSpan.FromHours(2)), moment.Value);

        var draft = DocumentOf(field);
        draft.Set("created_at", moment);
        Assert.Equal("""{"created_at":"2026-09-12T14:30:00Z"}""", draft.ToPatch());
    }

    [Fact]
    public void BoolParsesFormatsAndPatchesAsJsonBoolean()
    {
        var field = new FieldModel("is_return", "Return", FieldKind.Bool);
        Assert.True(field.TryParse("true", out var value, out var error));
        Assert.Null(error);
        Assert.True(Assert.IsType<BoolFieldValue>(value).Value);
        Assert.False(field.TryParse("1", out _, out error));
        Assert.Equal("field.invalid_bool", error!.Code);

        var draft = DocumentOf(field);
        draft.Set("is_return", new BoolFieldValue(true));
        Assert.Equal("""{"is_return":true}""", draft.ToPatch());
    }

    [Fact]
    public void SelectParsesOnlyAmongOptionsAndPatches()
    {
        var field = new FieldModel("status", "Status", FieldKind.Select, options: ["Draft", "Paid"]);
        Assert.True(field.TryParse("Paid", out var value, out var error));
        Assert.Null(error);
        Assert.Equal("Paid", Assert.IsType<SelectFieldValue>(value).Value);
        Assert.False(field.TryParse("paid", out _, out error));
        Assert.Equal("field.not_an_option", error!.Code);

        var draft = DocumentOf(field);
        draft.Set("status", value!);
        Assert.Equal("""{"status":"Paid"}""", draft.ToPatch());
    }

    [Fact]
    public void LinkParsesAndPatches()
    {
        var field = new FieldModel("customer", "Customer", FieldKind.Link, linkTo: "customer");
        Assert.True(field.TryParse("CUST-0001", out var value, out var error));
        Assert.Null(error);
        var link = Assert.IsType<LinkFieldValue>(value);
        Assert.Equal("CUST-0001", link.Id);
        Assert.Null(link.DisplayText);

        var draft = DocumentOf(field);
        draft.Set("customer", link with { DisplayText = "Acme GmbH" });
        Assert.Equal("""{"customer":"CUST-0001"}""", draft.ToPatch());
    }

    [Fact]
    public void AttachParsesAndPatches()
    {
        var field = new FieldModel("attachments", "Attachments", FieldKind.Attach);
        Assert.True(field.TryParse("ATT-0009", out var value, out var error));
        Assert.Null(error);
        Assert.Equal("ATT-0009", Assert.IsType<AttachFieldValue>(value).FileId);

        var draft = DocumentOf(field);
        draft.Set("attachments", value!);
        Assert.Equal("""{"attachments":"ATT-0009"}""", draft.ToPatch());
    }

    [Fact]
    public void JsonParsesEmbeddedAndPatchesEmbedded()
    {
        var field = new FieldModel("settings_json", "Settings", FieldKind.Json);
        Assert.True(field.TryParse("""{"keep":true}""", out var value, out var error));
        Assert.Null(error);
        var json = Assert.IsType<JsonFieldValue>(value);
        Assert.True(json.SemanticEquals(new JsonFieldValue("""{ "keep": true }""")));
        Assert.False(field.TryParse("{nope", out _, out error));
        Assert.Equal("field.invalid_json", error!.Code);

        var draft = DocumentOf(field);
        draft.Set("settings_json", json);
        Assert.Equal("""{"settings_json":{"keep":true}}""", draft.ToPatch());
    }

    [Fact]
    public void BytesParsesBase64AndPatches()
    {
        var field = new FieldModel("bytes_field", "Bytes", FieldKind.Bytes);
        Assert.True(field.TryParse("AAAA", out var value, out var error));
        Assert.Null(error);
        Assert.Equal("AAAA", Assert.IsType<BytesFieldValue>(value).Base64);
        Assert.False(field.TryParse("!!", out _, out error));
        Assert.Equal("field.invalid_base64", error!.Code);

        var draft = DocumentOf(field);
        draft.Set("bytes_field", value!);
        Assert.Equal("""{"bytes_field":"AAAA"}""", draft.ToPatch());
    }

    [Fact]
    public void SecretIsNeverTypedIntoADraft()
    {
        var field = new FieldModel("password", "Password", FieldKind.Secret);
        Assert.False(field.TryParse("hunter2", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.secret_not_editable", error!.Code);
    }

    [Fact]
    public void TableIsEditedAsRowsNotText()
    {
        var field = new FieldModel("taxes", "Taxes", FieldKind.Table);
        Assert.False(field.TryParse("[{}]", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.table_not_text", error!.Code);
    }

    [Fact]
    public void UnknownKindIsNeverEditablyTyped()
    {
        // A newer server's kind: parsing typed text refuses with a dedicated error
        // instead of guessing an encoding.
        var field = new FieldModel("signature", "Signature", FieldKind.Unknown, unknownKindName: "signature_pad");
        Assert.False(field.TryParse("handwritten", out var value, out var error));
        Assert.Null(value);
        Assert.Equal("field.unknown_kind", error!.Code);

        // The codec still keeps a loaded raw value as data (shown, never edited).
        var decoded = DecodeWire("\"raw-ink\"", FieldKind.Unknown);
        Assert.IsType<TextFieldValue>(decoded);
    }

    [Fact]
    public void EmptyTextParsesToNullForEveryKind()
    {
        var kinds = new[]
        {
            FieldKind.Bool, FieldKind.IntegerNumber, FieldKind.DecimalNumber, FieldKind.Text,
            FieldKind.Date, FieldKind.Time, FieldKind.Datetime, FieldKind.Link, FieldKind.Select,
            FieldKind.Json, FieldKind.Bytes, FieldKind.Attach,
        };
        foreach (var kind in kinds)
        {
            var field = new FieldModel("f", "F", kind);
            Assert.True(field.TryParse(null, out var value, out var error), $"{kind} parses null.");
            Assert.Null(error);
            Assert.Equal(FieldValue.Null, value);
            Assert.True(field.TryParse(string.Empty, out value, out error), $"{kind} parses empty.");
            Assert.Null(error);
            Assert.Equal(FieldValue.Null, value);
        }
    }

    [Fact]
    public void DecodeFormatsBackEveryKindFromItsContractEncoding()
    {
        // The document side of the round trip: the contract's wire texts decode to the
        // typed values that encode back to the same JSON. String-encoded kinds write
        // quoted JSON strings, int and bool write JSON numbers/booleans, JSON embeds.
        AssertRoundTrip(FieldKind.Text, "hello", "\"hello\"");
        AssertRoundTrip(FieldKind.DecimalNumber, "1250.00", "\"1250.00\"");
        AssertRoundTrip(FieldKind.IntegerNumber, "9007199254740993", "9007199254740993");
        AssertRoundTrip(FieldKind.Date, "2026-09-12", "\"2026-09-12\"");
        AssertRoundTrip(FieldKind.Time, "14:30:59", "\"14:30:59\"");
        AssertRoundTrip(FieldKind.Datetime, "2026-09-12T10:30:00Z", "\"2026-09-12T10:30:00Z\"");
        AssertRoundTrip(FieldKind.Bool, "true", "true");
        AssertRoundTrip(FieldKind.Bool, "false", "false");
        AssertRoundTrip(FieldKind.Select, "Paid", "\"Paid\"");
        AssertRoundTrip(FieldKind.Link, "CUST-0001", "\"CUST-0001\"");
        AssertRoundTrip(FieldKind.Attach, "ATT-0009", "\"ATT-0009\"");
        AssertRoundTrip(FieldKind.Bytes, "AAAA", "\"AAAA\"");
        // Json embeds: the wire document is the object itself, never a quoted string.
        Assert.Equal("{\"a\":1}", WriteToString(DecodeWire("{\"a\":1}", FieldKind.Json)));
    }

    private static void AssertRoundTrip(FieldKind kind, string wire, string expected)
    {
        var decoded = DecodeWire($"\"{wire}\"", kind);
        Assert.Equal(expected, WriteToString(decoded));
    }

    /// <summary>Decodes a wire JSON literal through the codec's public element path.</summary>
    private static FieldValue DecodeWire(string json, FieldKind kind)
    {
        using var document = JsonDocument.Parse(json);
        return ValueCodec.DecodeElement(document.RootElement, kind);
    }

    /// <summary>Writes a value's contract encoding to text through the codec's writer.</summary>
    private static string WriteToString(FieldValue value)
    {
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            ValueCodec.WriteValue(writer, value);
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private static DocumentDraft DocumentOf(FieldModel field) => new(
        new DocTypeModel(
            "round_trip",
            "Round Trip",
            version: 1,
            isPosted: false,
            fields: [field]));
}