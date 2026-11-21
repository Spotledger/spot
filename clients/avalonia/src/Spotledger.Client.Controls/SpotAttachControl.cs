using System.Globalization;
using Avalonia;
using Avalonia.Controls;
using Avalonia.Data;
using Avalonia.Input;
using Avalonia.Platform.Storage;
using Avalonia.Threading;

namespace Spotledger.Client.Controls;

/// <summary>File reference editor. Local files and streams exist only for the duration of an upload.</summary>
public sealed class SpotAttachControl : CatalogueControl, IDisposable
{
    public static readonly StyledProperty<string?> FileIdProperty = AvaloniaProperty.Register<SpotAttachControl, string?>(nameof(FileId), defaultBindingMode: BindingMode.TwoWay);
    public string? FileId { get => GetValue(FileIdProperty); set => SetValue(FileIdProperty, value); }
    public IFileUploader? Uploader { get; set; }
    /// <summary>The server's resolved maximum, or no client size check.</summary>
    public long? MaximumBytes { get; set; }
    /// <summary>The server's accepted media types, or no client type check.</summary>
    public IReadOnlyList<string>? AllowedContentTypes { get; set; }
    public decimal Progress { get; private set; }
    public bool IsUploading { get; private set; }
    public string? ErrorText { get; private set; }
    private CancellationTokenSource? _cancellation;
    private int _generation;
    private readonly TextBox _display = Display("File reference");
    private readonly TextBox _status = Display("Upload status");
    private readonly Button _choose;
    private readonly Button _cancel;
    private readonly Button _remove;
    public SpotAttachControl() : base("Attachment")
    {
        _choose = Action("Choose or replace file", () => _ = ChooseAsync());
        _cancel = Action("Cancel upload", Cancel);
        _remove = Action("Remove attachment", Remove);
        Body.Children.Add(_display);
        Body.Children.Add(_choose);
        Body.Children.Add(_status);
        Body.Children.Add(_cancel);
        Body.Children.Add(_remove);
        DragDrop.SetAllowDrop(this, true);
        AddHandler(DragDrop.DragOverEvent, (_, e) => { e.DragEffects = IsReadOnly ? DragDropEffects.None : DragDropEffects.Copy; e.Handled = true; });
        AddHandler(DragDrop.DropEvent, (_, e) =>
        {
            var file = e.DataTransfer.TryGetFiles()?.OfType<IStorageFile>().FirstOrDefault();
            if (file is not null && !IsReadOnly) { _ = UploadStorageFileAsync(file); e.Handled = true; }
        });
        UpdatePresentation();
    }
    public async Task ChooseAsync()
    {
        if (IsReadOnly || IsUploading) return;
        try
        {
            var storage = TopLevel.GetTopLevel(this)?.StorageProvider;
            if (storage is null || !storage.CanOpen) { ErrorText = "File picker is unavailable."; return; }
            var files = await storage.OpenFilePickerAsync(new FilePickerOpenOptions { AllowMultiple = false, Title = "Choose attachment" });
            if (files.Count > 0 && !IsReadOnly) await UploadStorageFileAsync(files[0]);
        }
        catch (Exception) { ErrorText = "Could not open the file. Try again."; }
        finally { UpdatePresentation(); }
    }
    private async Task UploadStorageFileAsync(IStorageFile file)
    {
        try
        {
            var properties = await file.GetBasicPropertiesAsync();
            await using var stream = await file.OpenReadAsync();
            await UploadAsync(new UploadFile(file.Name, MediaTypeForName(file.Name), properties.Size is { } size && size <= long.MaxValue ? (long)size : null, stream));
        }
        catch (Exception) { ErrorText = "Could not read the file. Try again."; UpdatePresentation(); }
    }
    /// <summary>Advisory media type for chooser/drop sources that expose no MIME metadata.
    /// Unknown extensions remain octet-stream; the upload service is still authoritative.</summary>
    public static string MediaTypeForName(string name) => System.IO.Path.GetExtension(name).ToUpperInvariant() switch
    {
        ".PNG" => "image/png",
        ".JPG" or ".JPEG" => "image/jpeg",
        ".GIF" => "image/gif",
        ".WEBP" => "image/webp",
        ".PDF" => "application/pdf",
        ".TXT" => "text/plain",
        ".CSV" => "text/csv",
        ".JSON" => "application/json",
        ".ZIP" => "application/zip",
        _ => "application/octet-stream",
    };
    public async Task UploadAsync(UploadFile file)
    {
        ArgumentNullException.ThrowIfNull(file);
        if (IsReadOnly) return;
        Cancel();
        if (!CheckLimits(file)) { UpdatePresentation(); return; }
        if (Uploader is null) { ErrorText = "File uploader is not configured."; UpdatePresentation(); return; }
        _cancellation = new CancellationTokenSource();
        var token = _cancellation.Token;
        var generation = ++_generation;
        IsUploading = true;
        Progress = 0m;
        ErrorText = null;
        UpdatePresentation();
        try
        {
            var id = await Uploader.UploadAsync(file, new UploadProgress(value =>
            {
                void Update() { if (generation == _generation && !token.IsCancellationRequested) { Progress = Math.Clamp(value, 0m, 1m); UpdatePresentation(); } }
                if (Dispatcher.UIThread.CheckAccess()) Update(); else Dispatcher.UIThread.Post(Update);
            }), token);
            if (generation != _generation || token.IsCancellationRequested || IsReadOnly) return;
            if (string.IsNullOrWhiteSpace(id)) { ErrorText = "Upload returned no file reference."; return; }
            SetCurrentValue(FileIdProperty, id);
            Progress = 1m;
        }
        catch (OperationCanceledException) when (token.IsCancellationRequested) { }
        catch (Exception) { if (generation == _generation) ErrorText = "Upload failed. Try again."; }
        finally { if (generation == _generation) { IsUploading = false; UpdatePresentation(); } }
    }
    private bool CheckLimits(UploadFile file)
    {
        if (MaximumBytes is { } maximum)
        {
            var size = file.Size ?? (file.Content.CanSeek ? file.Content.Length - file.Content.Position : (long?)null);
            if (size is null || size < 0 || size > maximum)
            {
                ErrorText = "Maximum attachment size: " + maximum.ToString(CultureInfo.InvariantCulture) + " bytes."
                    + (size is null ? " File size could not be checked." : string.Empty);
                return false;
            }
        }
        if (AllowedContentTypes is { Count: > 0 } accepted && !accepted.Any(type =>
                string.Equals(type, file.ContentType, StringComparison.OrdinalIgnoreCase)
                || (type.EndsWith("/*", StringComparison.Ordinal)
                    && file.ContentType.StartsWith(type[..^1], StringComparison.OrdinalIgnoreCase))))
        {
            ErrorText = "Accepted attachment types: " + string.Join(", ", accepted) + ".";
            return false;
        }
        return true;
    }
    public void Cancel()
    {
        _generation++;
        _cancellation?.Cancel();
        _cancellation?.Dispose();
        _cancellation = null;
        IsUploading = false;
        UpdatePresentation();
    }
    public void Remove() { if (!IsReadOnly) { Cancel(); SetCurrentValue(FileIdProperty, null); } }
    public void Dispose() => Cancel();
    protected override void OnDetachedFromVisualTree(VisualTreeAttachmentEventArgs e) { Cancel(); base.OnDetachedFromVisualTree(e); }
    protected override void OnPropertyChanged(AvaloniaPropertyChangedEventArgs change)
    {
        base.OnPropertyChanged(change);
        if (change.Property == FileIdProperty) UpdatePresentation();
        if (change.Property == IsReadOnlyProperty && IsReadOnly && IsUploading) Cancel();
    }
    protected override void UpdatePresentation()
    {
        if (_choose is null) return;
        _display.Text = FileId ?? "No file";
        _status.Text = ErrorText ?? (IsUploading ? (Progress * 100m).ToString("0", CultureInfo.InvariantCulture) + "%" : string.Empty);
        _status.IsVisible = IsUploading || ErrorText is not null;
        _choose.IsVisible = !IsReadOnly && !IsUploading;
        _cancel.IsVisible = IsUploading && !IsReadOnly;
        _remove.IsVisible = FileId is not null && !IsReadOnly;
    }
    private sealed class UploadProgress(Action<decimal> report) : IProgress<decimal>
    {
        public void Report(decimal value) => report(value);
    }
}
