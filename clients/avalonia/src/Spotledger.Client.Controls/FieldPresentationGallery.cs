using System.Windows.Input;
using Avalonia.Controls;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Controls;

/// <summary>ADR-068 A2 fixtures; server inputs and app commands below are explicitly mock.</summary>
public sealed class FieldPresentationGallery : StackPanel
{
    public FieldPresentationGallery()
    {
        foreach (var revealable in new[] { false, true })
        {
            var field = new FieldModel("value", "Protected", FieldKind.Text);
            var draft = new DocumentDraft(new DocTypeModel("demo", "Demo", 1, false, [field]));
            draft.ApplyPermissions(PermissionSnapshot.Decode(revealable
                ? """{"etag":"demo","data":{"doctypes":[{"doctype":"demo","fields":{"masked":["value"],"revealable":["value"]}}]}}"""
                : """{"etag":"demo","data":{"doctypes":[{"doctype":"demo","fields":{"masked":["value"]}}]}}"""));
            draft.LoadFrom("""{"value":"Server mask …42"}""");
            var masked = (SpotMaskedField)FieldControlFactory.Create(field, draft.Fields["value"]);
            masked.RevealCommand = new GalleryCommand(() =>
            {
                draft.ApplyRevealResponse("value", """{"data":{"value":"Mock reveal (read-only)"}}""");
                masked.Load(draft.Fields["value"]);
            });
            Add(revealable ? "Masked — reveal permitted (mock)" : "Masked — no reveal right", masked);
            if (revealable)
            {
                var hide = new Button { Content = "Hide mock reveal" };
                hide.Click += (_, _) => masked.HideReveal();
                Children.Add(hide);
            }
        }
        Add("Currency — server symbol", new SpotCurrencyInput { CurrencyInfo = new("PKR", "Rs"), Text = "123456789012345678901234567890.12" });
        Add("Currency — absent symbol", new SpotCurrencyInput { Text = "123.45" });
        Add("Currency — read only", new SpotCurrencyInput { CurrencyInfo = new("USD", "$"), Text = "12.34", IsReadOnly = true });
        Add("Currency — invalid", new SpotCurrencyInput { CurrencyInfo = new("PKR", "Rs"), Text = "12.345", ErrorText = "At most 2 decimal places." });
        var limits = new SpotAttachControl { MaximumBytes = 3, AllowedContentTypes = ["text/plain"] };
        limits.AttachedToVisualTree += (_, _) => _ = limits.UploadAsync(new("demo.png", "image/png", 3, System.IO.Stream.Null));
        Add("Attachment — type refused before upload", limits);
        var size = new SpotAttachControl { MaximumBytes = 3 };
        size.AttachedToVisualTree += (_, _) => _ = size.UploadAsync(new("demo.txt", "text/plain", 4, System.IO.Stream.Null));
        Add("Attachment — size refused before upload", size);
        foreach (var hint in Enum.GetValues<ControlHint>())
        {
            var field = hint is ControlHint.Switch or ControlHint.Checkbox
                ? new FieldModel("flag", "Flag", FieldKind.Bool, hints: new(Control: hint))
                : hint == ControlHint.Textarea
                    ? new FieldModel("note", "Note", FieldKind.Text, hints: new(Control: hint))
                    : new FieldModel("options", "Options", FieldKind.Select, options: ["One", "Two"], multiple: hint == ControlHint.Chips, hints: new(Control: hint));
            Add("Hint — " + hint, FieldControlFactory.Create(field));
        }
        Add("Multiple options — selected", new SpotSelectMultiple { Options = ["One", "Two", "Three"], SelectedValues = ["One", "Three"] });
        Add("Multiple options — read only", new SpotSelectMultiple { Options = ["One", "Two", "Three"], SelectedValues = ["One", "Three"], IsReadOnly = true });
        var createStatus = new TextBlock { Text = "Mock create has not been requested" };
        var command = new GalleryCommand(() => createStatus.Text = "Mock create requested — host opens the workflow");
        var lookup = new SpotLookupBox { Source = new GalleryColumns(), Delay = new GalleryClock(), CanCreateTarget = true, CreateCommand = command };
        lookup.AttachedToVisualTree += (_, _) => { lookup.SearchText = null; lookup.SearchText = "demo"; };
        Add("Lookup — columns and create right", lookup);
        var multi = new SpotMultiSelect { Source = new GalleryColumns(), CanCreateTarget = true, CreateCommand = command };
        multi.Lookup.Delay = new GalleryClock();
        multi.AttachedToVisualTree += (_, _) => { multi.Lookup.SearchText = null; multi.Lookup.SearchText = "demo"; };
        Add("Multiple links — columns and create right", multi);
        Children.Add(createStatus);
    }
    private void Add(string label, Control control) => Children.Add(new SpotField { Label = label, Content = control });
    private sealed class GalleryCommand(Action action) : ICommand
    {
        public bool CanExecute(object? parameter) => true;
        public void Execute(object? parameter) => action();
        public event EventHandler? CanExecuteChanged { add { } remove { } }
    }
    private sealed class GalleryClock : ILookupDelay
    {
        public Task WaitAsync(TimeSpan delay, CancellationToken token) => Task.CompletedTask;
    }
    private sealed class GalleryColumns : ILookupSource
    {
        public Task<IReadOnlyList<LookupItem>> SearchAsync(string text, CancellationToken cancellationToken) => Task.FromResult<IReadOnlyList<LookupItem>>([]);
        public Task<LookupItem?> ResolveAsync(string id, CancellationToken cancellationToken) => Task.FromResult<LookupItem?>(null);
        public Task<LookupResult> SearchResultAsync(string text, CancellationToken cancellationToken) => Task.FromResult(LookupResult.Decode("""
            {"data":{"columns":[{"name":"code","label":"Code / کوڈ"},{"name":"region","label":"Region / خطہ"}],"rows":[
            {"id":"a","label":"اکمی / Acme","cells":{"code":"A-42","region":"East / مشرق"}},
            {"id":"b","label":"کریم / Karim","cells":{"code":"B-43","region":"West / مغرب"}}],"has_more":false}}
            """));
    }
}
