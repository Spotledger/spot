using Avalonia.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>State fixtures for the precompiled catalogue gallery. All transports are explicitly mock.</summary>
public sealed class CatalogueGallery : StackPanel
{
    public CatalogueGallery()
    {
        foreach (var state in new[] { "empty", "selected", "loading", "error", "stale", "readonly" })
        {
            var lookup = new SpotLookupBox { Source = new GalleryLookupSource(state), Delay = new GalleryDelay() };
            if (state is "selected" or "readonly") lookup.SelectedItem = new("a", "اکمی / Acme", "A saved reference");
            else if (state == "stale") _ = lookup.SetSelectionAsync(new("gone", "Saved reference"));
            else lookup.AttachedToVisualTree += (_, _) => { lookup.SearchText = null; lookup.SearchText = state; };
            lookup.IsReadOnly = state == "readonly";
            Add("Lookup — " + state, lookup);
        }
        Add("Multiple links — empty", new SpotMultiSelect { Source = new GalleryLookupSource("selected") });
        Add("Multiple links — chips", new SpotMultiSelect { Source = new GalleryLookupSource("selected"), SelectedItems = [new("a", "اکمی"), new("b", "کریم")] });
        Add("Multiple links — read only", new SpotMultiSelect { IsReadOnly = true, SelectedItems = [new("a", "اکمی"), new("b", "کریم")] });
        Add("Long text — empty", new SpotTextArea());
        Add("Long text — auto grow", new SpotTextArea { Text = "First line\nSecond line\nThird line" });
        Add("Code", new SpotTextArea { Text = "value = 42", Format = "code" });
        Add("Markdown — editor", new SpotTextArea { Text = "# Notes\n**Important**", Format = "markdown" });
        Add("Markdown — preview", new SpotTextArea { Text = "# Notes\n**Important**", Format = "markdown", IsPreviewOpen = true });
        Add("Long text — read only", new SpotTextArea { Text = "Selectable saved notes", IsReadOnly = true });
        Add("Secret — not set", new SpotSecretInput());
        Add("Secret — set", new SpotSecretInput { Value = new(true) });
        Add("Secret — read only", new SpotSecretInput { Value = new(true), IsReadOnly = true });
        Add("Time — empty", new SpotTimeInput());
        Add("Time — set", new SpotTimeInput { Value = new TimeOnly(14, 30) });
        Add("Time — invalid", new SpotTimeInput { Text = "25:61", ErrorText = "Enter a valid time." });
        Add("Time — read only", new SpotTimeInput { Value = new TimeOnly(14, 30), IsReadOnly = true });
        var instant = new DateTimeOffset(2026, 10, 15, 14, 30, 0, TimeSpan.FromHours(5));
        Add("Date/time — empty", new SpotDateTimeInput());
        Add("Date/time — set", new SpotDateTimeInput { Value = instant });
        Add("Date/time — invalid", new SpotDateTimeInput { Text = "2026-10-15 14:30", ErrorText = "An explicit offset is required." });
        Add("Date/time — read only", new SpotDateTimeInput { Value = instant, IsReadOnly = true });
        Add("Percent — empty", new SpotPercentInput());
        Add("Percent — precise", new SpotPercentInput { Value = 12.125m, Scale = 3 });
        Add("Percent — invalid", new SpotPercentInput { ErrorText = "At most 2 decimal places." });
        Add("Percent — read only", new SpotPercentInput { Value = 12.125m, Scale = 3, IsReadOnly = true });
        var rowModel = new DocTypeModel("demo_row", "Demo row", 1, false, [new("note", "Note", FieldKind.Text)]);
        var rows = new TableFieldValue([TableRowValue.New(new Dictionary<string, FieldValue> { ["note"] = new TextFieldValue("Editable child note") })], rowModel);
        Add("Rows — empty", new SpotTableControl { Value = new TableFieldValue([], rowModel) });
        Add("Rows — editable", new SpotTableControl { Value = rows });
        Add("Rows — read only", new SpotTableControl { Value = rows, IsReadOnly = true });
        foreach (var state in new[] { "empty", "selected", "loading", "error", "readonly" })
        {
            var attach = new SpotAttachControl { Uploader = new GalleryUploader(state) };
            if (state is "selected" or "readonly") attach.FileId = "demo-file-id";
            if (state is "loading" or "error") attach.AttachedToVisualTree += (_, _) => _ = attach.UploadAsync(new UploadFile("demo.txt", "text/plain", 0, System.IO.Stream.Null));
            attach.IsReadOnly = state == "readonly";
            Add("Attachment — " + state, attach);
        }
        Children.Add(new FieldPresentationGallery());
    }
    private void Add(string label, Control control) => Children.Add(new SpotField { Label = label, Content = control });
    private sealed class GalleryDelay : ILookupDelay
    {
        public Task WaitAsync(TimeSpan delay, CancellationToken token) => Task.CompletedTask;
    }
    private sealed class GalleryLookupSource(string state) : ILookupSource
    {
        public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken) => state switch
        {
            "loading" => WaitForCancel(cancellationToken),
            "error" => Task.FromException<IReadOnlyList<LookupItem>>(new InvalidOperationException("Mock lookup failure")),
            "empty" => Task.FromResult<IReadOnlyList<LookupItem>>([]),
            _ => Task.FromResult<IReadOnlyList<LookupItem>>([new("a", "اکمی / Acme", "Candidate description"), new("b", "کریم / Karim", "Second candidate")]),
        };
        private static async Task<IReadOnlyList<LookupItem>> WaitForCancel(CancellationToken token) { await Task.Delay(Timeout.Infinite, token); return []; }
        public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => Task.FromResult<LookupItem?>(null);
    }
    private sealed class GalleryUploader(string state) : IFileUploader
    {
        public async Task<string> UploadAsync(UploadFile file, IProgress<decimal> progress, CancellationToken cancellationToken)
        {
            if (state == "error") throw new InvalidOperationException("Mock upload failure");
            progress.Report(0.45m);
            await Task.Delay(Timeout.Infinite, cancellationToken);
            return "demo-file-id";
        }
    }
}
