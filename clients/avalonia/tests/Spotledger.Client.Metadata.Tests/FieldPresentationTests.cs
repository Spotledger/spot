using System.Net;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Api;
using Xunit;

namespace Spotledger.Client.Metadata.Tests;

public sealed class FieldPresentationTests
{
    [Fact]
    public void FormatParametersAndMultipleDecodeFromRealWireShape()
    {
        var model = MetadataResponseDecoder.Decode("""
            {"data":{"doctype":"record","label":"Record","version":1,"fields":[
              {"name":"amount","label":"Amount","type":"decimal","format":"currency","format_params":{"currency_field":"currency"}},
              {"name":"barcode","label":"Barcode","type":"text","format_params":{"symbology":"code128"}},
              {"name":"code","label":"Code","type":"long_text","format_params":{"syntax":"rust"}},
              {"name":"rating","label":"Rating","type":"int","format_params":{"max":9}},
              {"name":"tags","label":"Tags","type":"text","select":["a","b"],"multiple":true,"hints":{"control":"chips"}}
            ]}}
            """);
        Assert.Equal("currency", model.Field("amount")!.FormatParameters.CurrencyField);
        Assert.Equal("code128", model.Field("barcode")!.FormatParameters.Symbology);
        Assert.Equal("rust", model.Field("code")!.FormatParameters.Syntax);
        Assert.Equal(9L, model.Field("rating")!.FormatParameters.Max);
        Assert.True(model.Field("tags")!.Multiple);
        Assert.Equal(FieldKind.Select, model.Field("tags")!.Kind);
        Assert.Equal(ControlHint.Chips, model.Field("tags")!.Hints.Control);
        var draft = new DocumentDraft(model);
        draft.LoadFrom("""{"tags":["a"]}""");
        Assert.Equal(["a"], Assert.IsType<MultiSelectFieldValue>(draft.Get("tags")).Values);
        Assert.True(draft.TrySetText("tags", """["b","a","b"]"""));
        Assert.Equal("""{"tags":["a","b"]}""", draft.ToPatch());
        Assert.Empty(DraftValidator.Validate(draft));
        Assert.False(draft.TrySetText("tags", """["invalid"]"""));
    }

    [Theory]
    [InlineData("switch", ControlHint.Switch)]
    [InlineData("checkbox", ControlHint.Checkbox)]
    [InlineData("dropdown", ControlHint.Dropdown)]
    [InlineData("radio", ControlHint.Radio)]
    [InlineData("segmented", ControlHint.Segmented)]
    [InlineData("autocomplete", ControlHint.Autocomplete)]
    [InlineData("chips", ControlHint.Chips)]
    [InlineData("textarea", ControlHint.Textarea)]
    [InlineData("future_control", ControlHint.Unknown)]
    [InlineData("Switch", ControlHint.Unknown)]
    public async Task HintTextSurvivesTheActualCacheBoundary(string raw, ControlHint expected)
    {
        using var connection = new ApiConnection(new JsonHandler("""
            {"data":{"doctype":"record","label":"Record","version":1,"fields":[
            {"name":"text","label":"Text","type":"text","hints":{"control":"CONTROL"}}]}}
            """.Replace("CONTROL", raw, StringComparison.Ordinal)));
        connection.Connect(new Uri("https://example.test"));
        var field = (await new MetaCache(connection).GetAsync("record")).Fields[0];
        Assert.Equal(expected, field.Hints.Control);
        Assert.Equal(raw, field.Hints.RawControl);
        Assert.False(field.Multiple);
    }

    [Fact]
    public void MaskedTextNeverEntersValueOriginalOrSaveAndRevealIsSeparate()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.ApplyPermissions(Summary("p1"));
        draft.LoadFrom("""{"id":"one","version":1,"rate":"***.00","customer":"masked customer"}""");
        var state = draft.Fields["rate"];
        Assert.True(state.IsMasked);
        Assert.False(state.IsEditable);
        Assert.True(state.CanReveal);
        Assert.Equal("***.00", state.MaskedText);
        Assert.IsType<NullFieldValue>(state.Value);
        Assert.IsType<NullFieldValue>(state.Original);
        Assert.IsType<NullFieldValue>(draft.Get("rate"));
        Assert.Throws<InvalidOperationException>(() => draft.Set("rate", new TextFieldValue("***.00")));
        Assert.Throws<InvalidOperationException>(() => state.Value = new TextFieldValue("bypass"));
        Assert.Equal("{}", draft.ToPatch());
        draft.ApplyRevealResponse("rate", """{"data":{"value":"123456789012345678901234567890.00"}}""");
        Assert.Equal("123456789012345678901234567890.00", Assert.IsType<DecimalFieldValue>(state.RevealedValue).CanonicalText);
        Assert.IsType<NullFieldValue>(state.Value);
        Assert.Equal("{}", draft.ToPatch());
        draft.Revert();
        Assert.Null(state.RevealedValue);
        Assert.Equal("***.00", state.MaskedText);
        draft.LoadSavedFrom("""{"id":"one","version":2,"rate":"new mask"}""");
        Assert.Equal("new mask", state.MaskedText);
        Assert.Null(state.RevealedValue);
    }

    [Fact]
    public void PermissionsAndLimitsRefreshByEtagAndCreateComesFromTheTarget()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        var events = 0;
        draft.StructureChanged += (_, _) => events++;
        draft.ApplyPermissions(Summary("p1"));
        Assert.Equal("p1", draft.PermissionETag);
        Assert.Equal(5_000_000_000L, draft.Fields["attachments"].AttachLimit!.MaxBytes);
        Assert.Equal(["image/*", "application/pdf"], draft.Fields["attachments"].AttachLimit!.Accept);
        Assert.True(draft.Fields["customer"].CanCreateTarget);
        Assert.False(draft.Fields["grand_total"].IsEditable);
        draft.LoadFrom("""{"rate":"***"}""");
        draft.ApplyRevealResponse("rate", """{"data":{"value":"12.5"}}""");
        draft.ApplyPermissions(Summary("p1"));
        Assert.NotNull(draft.Fields["rate"].RevealedValue);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"p2","data":{"doctypes":[
              {"doctype":"sales_invoice","rights":{"create":true},"fields":{"masked":["rate"],"revealable":[]},"attach_limits":{"attachments":{"max_bytes":99}}},
              {"doctype":"customer","rights":{"create":false},"fields":{}}]}}
            """));
        Assert.False(draft.Fields["customer"].CanCreateTarget);
        Assert.False(draft.Fields["rate"].CanReveal);
        Assert.Null(draft.Fields["rate"].RevealedValue);
        Assert.Equal(99, draft.Fields["attachments"].AttachLimit!.MaxBytes);
        Assert.Empty(draft.Fields["attachments"].AttachLimit!.Accept);
        Assert.Equal(3, events); // two changed snapshots and the document read
        Assert.Throws<InvalidOperationException>(() => draft.ApplyRevealResponse("rate", """{"data":{"value":"1"}}"""));
    }

    [Fact]
    public void OldSummaryDefaultsArePlainAndRevocationScrubsOldValues()
    {
        var draft = TestModels.LoadedInvoice();
        draft.ApplyPermissions(Summary("p1"));
        Assert.IsType<NullFieldValue>(draft.Fields["rate"].Original);
        Assert.Null(draft.Fields["rate"].MaskedText); // never relabel a previous real value as masked text
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"old","data":{"doctypes":[{"doctype":"sales_invoice","rights":{},"fields":{"hidden":[],"read_only":[],"masked":[]}}]}}
            """));
        Assert.All(draft.Fields.Values, state =>
        {
            Assert.False(state.IsMasked);
            Assert.False(state.CanReveal);
            Assert.False(state.CanCreateTarget);
            Assert.Null(state.AttachLimit);
            Assert.Null(state.RevealedValue);
            Assert.Null(state.MaskedText);
        });
        Assert.True(draft.Fields["rate"].IsEditable);
        Assert.IsType<NullFieldValue>(draft.Get("rate"));
        draft.LoadFrom(TestModels.DocumentJson);
        Assert.IsType<DecimalFieldValue>(draft.Get("rate"));
    }

    [Fact]
    public void CurrencyIsPerPathRefreshedOnReadAndSaveAndNeverWritten()
    {
        var draft = TestModels.LoadedInvoice();
        draft.LoadFrom("""
            {"version":10,"rate":"7.25","_currency":{"rate":{"code":"USD","symbol":"$"},"taxes[2].qty":{"code":"PKR","symbol":"Rs"}}}
            """);
        Assert.Equal(new CurrencyInfo("USD", "$"), draft.Currency["rate"]);
        Assert.Equal(new CurrencyInfo("PKR", "Rs"), draft.Currency["taxes[2].qty"]);
        Assert.DoesNotContain("_currency", draft.UnknownProperties.Keys);
        draft.Set("rate", new DecimalFieldValue(8m));
        Assert.Equal("""{"rate":"8"}""", draft.ToPatch());
        draft.LoadSavedFrom("""{"version":11,"rate":"8","_currency":{"rate":{"code":"EUR","symbol":"€"}}}""");
        Assert.Equal(new CurrencyInfo("EUR", "€"), Assert.Single(draft.Currency).Value);
        Assert.Equal(11, draft.Version);
        Assert.Equal("{}", draft.ToPatch());
        draft.LoadFrom("""{"version":12}""");
        Assert.Empty(draft.Currency);
    }

    [Fact]
    public void MaskedChildFieldsAreNeverDecodedAsScalarsAndStaySafeAcrossRevert()
    {
        var draft = new DocumentDraft(TestModels.Invoice());
        draft.ApplyPermissions(PermissionSnapshot.Decode("""
            {"etag":"children","data":{"doctypes":[{"doctype":"sales_invoice_item","rights":{},
            "fields":{"masked":["qty"],"revealable":["qty"]}}]}}
            """));
        draft.LoadFrom(TestModels.DocumentJson.Replace("10.00", "masked quantity", StringComparison.Ordinal));
        var table = draft.Table("taxes");
        var state = table.Rows[0].Values["qty"];
        Assert.Equal("masked quantity", state.MaskedText);
        Assert.IsType<NullFieldValue>(state.Value);
        Assert.IsType<NullFieldValue>(Assert.IsType<TableFieldValue>(draft.Get("taxes")).Rows[0].Values["qty"]);
        table.Rows[0].SetValue("item_code", new LinkFieldValue("changed", null));
        using var patch = JsonDocument.Parse(draft.ToPatch());
        Assert.All(patch.RootElement.GetProperty("taxes").EnumerateArray(), row => Assert.False(row.TryGetProperty("qty", out _)));
        draft.ApplyRevealResponse("taxes[1].qty", """{"data":{"value":"10.00"}}""");
        Assert.IsType<DecimalFieldValue>(state.RevealedValue);
        draft.Revert();
        Assert.Equal("masked quantity", table.Rows[0].Values["qty"].MaskedText);
        Assert.Null(table.Rows[0].Values["qty"].RevealedValue);
        Assert.Throws<InvalidOperationException>(() => table.AddRow().SetValue("qty", new DecimalFieldValue(5m)));
    }

    [Fact]
    public void LookupColumnsCellsAndLegacyFallbackAreIndependentFromExtras()
    {
        var result = LookupResult.Decode("""
            {"data":{"columns":[{"name":"code","label":"Code"},{"name":"region","label":"Region"}],
            "rows":[{"id":"one","label":"Chosen label","cells":{"code":"C1"},"extras":{"rate":"12.00"}},
            {"id":"two","label":"Second"}],"has_more":true,"cursor":"next"}}
            """);
        Assert.Equal([new LookupColumn("code", "Code"), new LookupColumn("region", "Region")], result.Columns);
        Assert.Equal("Chosen label", result.Rows[0].Label);
        Assert.Equal("C1", result.Rows[0].Cells["code"]);
        Assert.Equal("\"12.00\"", result.Rows[0].ExtrasJson["rate"]);
        Assert.Empty(result.Rows[1].Cells);
        Assert.True(result.HasMore);
        Assert.Equal("next", result.Cursor);
        var old = LookupResult.Decode("""{"data":{"rows":[{"id":"one","label":"One","description":"Description"}],"has_more":false}}""");
        Assert.Empty(old.Columns);
        Assert.Empty(old.Rows[0].Cells);
        Assert.Equal("Description", old.Rows[0].Description);
        Assert.False(old.HasMore);
        Assert.Null(old.Cursor);
    }

    private static PermissionSnapshot Summary(string etag) => PermissionSnapshot.Decode("""
        {"etag":"ETAG","data":{"doctypes":[
        {"doctype":"sales_invoice","rights":{"create":false},"fields":{"hidden":[],"read_only":[],"masked":["rate"],"revealable":["rate"]},
        "attach_limits":{"attachments":{"max_bytes":5000000000,"accept":["image/*","application/pdf"]}}},
        {"doctype":"customer","rights":{"create":true},"fields":{}}]}}
        """.Replace("ETAG", etag, StringComparison.Ordinal));

    private sealed class JsonHandler(string json) : HttpMessageHandler
    {
        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken) =>
            Task.FromResult(new HttpResponseMessage(HttpStatusCode.OK) { Content = new StringContent(json, Encoding.UTF8, "application/json") });
    }
}
