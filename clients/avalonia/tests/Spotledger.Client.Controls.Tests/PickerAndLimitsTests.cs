using Avalonia.Controls;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Avalonia.VisualTree;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class PickerAndLimitsTests
{
    [Theory]
    [InlineData("photo.PNG", "image/png")]
    [InlineData("photo.jpeg", "image/jpeg")]
    [InlineData("note.txt", "text/plain")]
    [InlineData("file.pdf", "application/pdf")]
    [InlineData("unknown.custom", "application/octet-stream")]
    public void ChooserMediaHintsDegradeConservatively(string name, string expected) =>
        Assert.Equal(expected, SpotAttachControl.MediaTypeForName(name));

    [AvaloniaFact]
    public async Task LimitsBindFromStateAndRejectBeforeInvokingUploader()
    {
        var field = new FieldModel("value", "File", FieldKind.Attach);
        var draft = FieldPresentationTests.Draft(field);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"limits","data":{"doctypes":[{"doctype":"demo","attach_limits":{"value":{"max_bytes":3,"accept":["text/plain"]}}}]}}"""));
        var input = Assert.IsType<SpotAttachControl>(FieldControlFactory.Create(field, draft.Fields["value"]));
        var uploader = new CountingUploader();
        input.Uploader = uploader;
        input.FileId = "saved";
        Assert.Equal(3, input.MaximumBytes);
        await input.UploadAsync(new("too-large", "text/plain", 4, Stream.Null));
        Assert.Equal(0, uploader.Calls);
        Assert.Contains("3 bytes", input.ErrorText);
        Assert.Equal("saved", input.FileId);
        await input.UploadAsync(new("wrong-type", "image/png", 3, Stream.Null));
        Assert.Equal(0, uploader.Calls);
        Assert.Contains("text/plain", input.ErrorText);
        await input.UploadAsync(new("boundary", "text/plain", 3, Stream.Null));
        Assert.Equal(1, uploader.Calls);
        Assert.Equal("uploaded", input.FileId);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"no-limits","data":{}}"""));
        FieldControlFactory.GetBinding(input).Load(draft.Fields["value"]);
        Assert.Null(input.MaximumBytes);
        Assert.Null(input.AllowedContentTypes);
        await input.UploadAsync(new("unchecked", "other/type", long.MaxValue, Stream.Null));
        Assert.Equal(2, uploader.Calls);
    }

    [AvaloniaFact]
    public async Task PublishedMediaSubtypeWildcardAllowsImagesButNotText()
    {
        var uploader = new CountingUploader();
        var input = new SpotAttachControl { Uploader = uploader, AllowedContentTypes = ["image/*"] };
        await input.UploadAsync(new("photo.png", "image/png", 1, Stream.Null));
        Assert.Equal(1, uploader.Calls);
        await input.UploadAsync(new("note.txt", "text/plain", 1, Stream.Null));
        Assert.Equal(1, uploader.Calls);
        Assert.Contains("image/*", input.ErrorText);
    }

    [AvaloniaFact]
    public async Task UnknownLengthIsCheckedWithoutReadingAndUnknownMediaDoesNotBypassLimits()
    {
        var uploader = new CountingUploader();
        var input = new SpotAttachControl { Uploader = uploader, MaximumBytes = 3 };
        using var large = new MemoryStream([1, 2, 3, 4]);
        await input.UploadAsync(new("a", "text/plain", null, large));
        Assert.Equal(0, uploader.Calls);
        Assert.Equal(0, large.Position);
        using var unknown = new UnknownLengthStream();
        await input.UploadAsync(new("b", "text/plain", null, unknown));
        Assert.Equal(0, uploader.Calls);
        Assert.Contains("could not be checked", input.ErrorText);
    }

    [AvaloniaFact]
    public void CreateCommandsFollowPermissionsAndDoNotOpenAnything()
    {
        var field = new FieldModel("value", "Reference", FieldKind.Link, linkTo: "target");
        var draft = FieldPresentationTests.Draft(field);
        var input = Assert.IsType<SpotLookupBox>(FieldControlFactory.Create(field, draft.Fields["value"]));
        var command = new RecordingCommand();
        input.CreateCommand = command;
        input.CreateCommandParameter = "target";
        input.RequestCreate();
        Assert.Equal(0, command.Calls);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"create","data":{"doctypes":[{"doctype":"target","rights":{"create":true}}]}}"""));
        FieldControlFactory.GetBinding(input).Load(draft.Fields["value"]);
        input.RequestCreate();
        Assert.Equal(1, command.Calls);
        Assert.Equal("target", command.LastParameter);
        command.Allowed = false;
        input.RequestCreate();
        Assert.Equal(1, command.Calls);
        command.Allowed = true;
        var multi = new SpotMultiSelect { CreateCommand = command, CanCreateTarget = true };
        multi.RequestCreate();
        Assert.Equal(2, command.Calls);
        multi.IsReadOnly = true;
        multi.RequestCreate();
        Assert.Equal(2, command.Calls);
        draft.ApplyPermissions(PermissionSnapshot.Decode("""{"etag":"revoked","data":{}}"""));
        FieldControlFactory.GetBinding(input).Load(draft.Fields["value"]);
        input.RequestCreate();
        Assert.Equal(2, command.Calls);
    }

    [AvaloniaFact]
    public async Task ColumnsRenderForBothPickersWhileKeyboardStoresLabelNotCell()
    {
        var source = new ColumnLookupSource();
        var input = new SpotLookupBox { Source = source, Delay = new ImmediateLookupDelay() };
        var window = TestHost.Show(input);
        input.SearchText = "typed";
        await input.PendingSearch;
        window.UpdateLayout();
        Assert.Equal(2, input.Columns.Count);
        var cells = input.GetVisualDescendants().OfType<TextBlock>().Select(text => text.Text).ToArray();
        Assert.Contains("Code", cells);
        Assert.Contains("Region", cells);
        Assert.Contains("DISPLAY-CELL", cells);
        input.HandleKey(Key.Down);
        input.HandleKey(Key.Enter);
        Assert.Equal("second-label", input.SelectedItem!.Label);
        Assert.Equal("second-id", input.SelectedItem.Id);
        window.Close();
        var multi = new SpotMultiSelect { Source = source };
        multi.Lookup.Delay = new ImmediateLookupDelay();
        window = TestHost.Show(multi);
        multi.Lookup.SearchText = "typed";
        await multi.Lookup.PendingSearch;
        window.UpdateLayout();
        Assert.Contains(multi.GetVisualDescendants().OfType<TextBlock>(), text => text.Text == "Region");
        multi.Lookup.HandleKey(Key.Enter);
        Assert.Equal("kept-label", Assert.Single(multi.SelectedItems).Label);
        Assert.Equal("first-id", Assert.Single(multi.SelectedItems).Id);
        window.Close();
    }

    private sealed class CountingUploader : IFileUploader
    {
        public int Calls { get; private set; }
        public Task<string> UploadAsync(UploadFile file, IProgress<decimal> progress, CancellationToken cancellationToken)
        { Calls++; return Task.FromResult("uploaded"); }
    }
    private sealed class UnknownLengthStream : Stream
    {
        public override bool CanRead => true;
        public override bool CanSeek => false;
        public override bool CanWrite => false;
        public override long Length => throw new NotSupportedException();
        public override long Position { get => throw new NotSupportedException(); set => throw new NotSupportedException(); }
        public override void Flush() => throw new NotSupportedException();
        public override int Read(byte[] buffer, int offset, int count) => throw new InvalidOperationException("Client check must not consume a stream.");
        public override long Seek(long offset, SeekOrigin origin) => throw new NotSupportedException();
        public override void SetLength(long value) => throw new NotSupportedException();
        public override void Write(byte[] buffer, int offset, int count) => throw new NotSupportedException();
    }
}
internal sealed class ColumnLookupSource : ILookupSource
{
    public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken) => throw new InvalidOperationException("Use renderer-neutral result.");
    public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => Task.FromResult<LookupItem?>(null);
    public Task<LookupResult> SearchResultAsync(string text, CancellationToken cancellationToken) => Task.FromResult(LookupResult.Decode("""
        {"data":{"columns":[{"name":"code","label":"Code"},{"name":"region","label":"Region"}],"rows":[
        {"id":"first-id","label":"kept-label","cells":{"code":"DISPLAY-CELL","region":"East"}},
        {"id":"second-id","label":"second-label","cells":{"code":"SECOND-CELL","region":"West"}}],"has_more":false}}
        """));
}
