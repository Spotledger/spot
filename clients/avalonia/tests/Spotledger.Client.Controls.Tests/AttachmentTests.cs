using Avalonia.Headless.XUnit;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class AttachmentTests
{
    [AvaloniaFact]
    public async Task UploadProgressReplaceRemoveAndLateCancellationPreserveOnlyTheFileId()
    {
        var uploader = new PendingUploader();
        var input = new SpotAttachControl { Uploader = uploader, FileId = "old" };
        TestHost.Show(input);
        Assert.Null(input.MaximumBytes);
        Assert.Null(input.AllowedContentTypes);
        var file = new UploadFile("file.txt", "text/plain", 3, new MemoryStream([1, 2, 3]));
        var upload = input.UploadAsync(file);
        uploader.Progress!.Report(0.5m);
        Assert.Equal(0.5m, input.Progress);
        Assert.True(input.IsUploading);
        input.Cancel();
        Assert.True(uploader.Token.IsCancellationRequested);
        uploader.Completion.SetResult("late");
        await upload;
        Assert.Equal("old", input.FileId);
        Assert.False(input.IsUploading);
        uploader.Completion = new();
        upload = input.UploadAsync(file);
        uploader.Completion.SetResult("new-id");
        await upload;
        Assert.Equal("new-id", input.FileId);
        input.IsReadOnly = true;
        input.Remove();
        Assert.Equal("new-id", input.FileId);
        input.IsReadOnly = false;
        input.Remove();
        Assert.Null(input.FileId);
    }

    [AvaloniaFact]
    public async Task FailureKeepsThePreviousReferenceAndShowsAnError()
    {
        var uploader = new PendingUploader();
        var input = new SpotAttachControl { Uploader = uploader, FileId = "old" };
        var task = input.UploadAsync(new UploadFile("a", "text/plain", 0, Stream.Null));
        uploader.Completion.SetException(new IOException("private-server-path"));
        await task;
        Assert.Equal("old", input.FileId);
        Assert.NotNull(input.ErrorText);
        Assert.DoesNotContain("private-server-path", input.ErrorText);
    }
}
internal sealed class PendingUploader : IFileUploader
{
    public TaskCompletionSource<string> Completion { get; set; } = new();
    public IProgress<decimal>? Progress { get; private set; }
    public CancellationToken Token { get; private set; }
    public Task<string> UploadAsync(UploadFile file, IProgress<decimal> progress, CancellationToken cancellationToken)
    {
        Progress = progress;
        Token = cancellationToken;
        return Completion.Task;
    }
}
