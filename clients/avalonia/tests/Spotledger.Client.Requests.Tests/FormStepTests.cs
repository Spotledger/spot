using Avalonia.Headless;
using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Api;
using Spotledger.Client.Forms;
using Spotledger.Client.Metadata;
using Spotledger.Client.Workspace;
using Xunit;

namespace Spotledger.Client.Requests.Tests;

public sealed class FormStepTests
{
    private static (FormStepViewModel Step, FakeRequests Requests, FakeDocuments Documents) Make(IDraftAutosave? autosave = null, RequestText? text = null)
    {
        var requests = new FakeRequests();
        var documents = new FakeDocuments();
        var step = new FormStepViewModel(
            requests,
            "T-1",
            info => new DocumentFormViewModel(documents, info.RecordType, info.RecordId, new NullFormShell(), autosave),
            text ?? Texts.English());
        return (step, requests, documents);
    }

    private static void Type(FormStepViewModel step, string title)
        => step.Form!.Draft!.Set("title", new TextFieldValue(title));

    [Fact]
    public async Task The_step_shows_its_title_instructions_and_the_draft_form()
    {
        var (step, requests, _) = Make();
        requests.Step = requests.Step with { WaitingFor = "Ben" };

        await step.LoadAsync();

        Assert.Equal(StepPhase.Open, step.Phase);
        Assert.Equal("Your details", step.Title);
        Assert.Equal("Fill in the title.", step.Instructions);
        Assert.Equal("Waiting for Ben", step.WaitingNotice);
        Assert.Equal("N-1", step.Form!.DocumentId);
    }

    [Fact]
    public async Task Complete_saves_the_draft_through_the_form_then_completes_the_step()
    {
        var (step, requests, documents) = Make();
        await step.LoadAsync();
        Type(step, "Taxi to the airport");
        var completed = 0;
        step.Completed += (_, _) => completed++;

        Assert.True(await step.CompleteAsync());

        Assert.Equal(StepPhase.Completed, step.Phase);
        Assert.Equal(1, completed);
        Assert.Contains(documents.Calls, c => c.Contains("Taxi to the airport", StringComparison.Ordinal));
        Assert.Equal(["complete T-1"], requests.Calls);
        Assert.Equal("Taxi to the airport", documents.Documents["N-1"]["title"]!.GetValue<string>());
    }

    [Fact]
    public async Task A_required_field_left_empty_keeps_the_step_open_and_never_calls_the_server()
    {
        var (step, requests, _) = Make();
        await step.LoadAsync();

        Assert.False(await step.CompleteAsync());

        Assert.Equal(StepPhase.Open, step.Phase);
        Assert.Empty(requests.Calls);
        Assert.Contains("title", step.FieldProblems().Keys);
        Assert.Equal("Some answers need attention before this step can be completed.", step.Status);
    }

    [Fact]
    public async Task A_server_refusal_maps_its_field_errors_to_fields_and_keeps_the_step_open()
    {
        var (step, requests, _) = Make();
        await step.LoadAsync();
        Type(step, "x");
        requests.CompleteError = new ApiError
        {
            StatusCode = 422,
            Code = "bpm_entry.form_invalid",
            Message = "invalid",
            FieldErrors = [new ApiFieldError("title", "value.too_short", "The title is too short.")],
        };

        Assert.False(await step.CompleteAsync());

        Assert.Equal(StepPhase.Open, step.Phase);
        Assert.Equal("The title is too short.", step.FieldProblems()["title"].Single());
        Assert.True(step.CanComplete);
    }

    [Fact]
    public async Task A_step_that_is_no_longer_open_says_so_in_plain_words()
    {
        var (step, requests, _) = Make();
        await step.LoadAsync();
        Type(step, "x");
        requests.CompleteError = new ApiError { StatusCode = 409, Code = "bpm_entry.step_not_open", Message = "bpm_entry.step_not_open" };

        Assert.False(await step.CompleteAsync());

        Assert.Equal(StepPhase.Unavailable, step.Phase);
        Assert.Equal("This step is no longer open.", step.Status);
    }

    [Fact]
    public async Task Typing_reaches_the_workspace_autosave_through_the_draft_seam()
    {
        var store = new InMemoryDraftStore();
        using var autosave = new AutosaveCoordinator(store, new DraftScope("https://erp.example", "ama"));
        var (step, _, _) = Make(autosave);
        await step.LoadAsync();

        Type(step, "Half written");
        await autosave.FlushAllAsync();

        var drafts = await store.ListAsync(autosave.Scope);
        Assert.Single(drafts);
    }

    [AvaloniaFact]
    public async Task A_step_is_completed_from_the_keyboard_alone()
    {
        var (step, requests, _) = Make();
        await step.LoadAsync();
        Type(step, "Keyboard only");
        using var view = new FormStepView(step);
        var window = TestHost.Show(view);

        window.KeyPress(Key.Enter, RawInputModifiers.Control, PhysicalKey.Enter, null);
        TestHost.Pump();
        for (var i = 0; i < 50 && step.Phase != StepPhase.Completed; i++)
        {
            await Task.Delay(20);
            TestHost.Pump();
        }

        Assert.Equal(StepPhase.Completed, step.Phase);
        Assert.Equal(["complete T-1"], requests.Calls);
        window.Close();
    }

    [AvaloniaFact]
    public async Task The_screen_labels_come_from_the_label_catalogue_and_flow_right_to_left_in_Arabic()
    {
        var (step, _, _) = Make(text: Texts.For("ar"));
        await step.LoadAsync();
        using var view = new FormStepView(step);
        var window = TestHost.Show(view);

        Assert.Equal(Avalonia.Media.FlowDirection.RightToLeft, view.FlowDirection);
        var texts = MyRequestsTests.AllText(view);
        Assert.Contains("حفظ", texts.Concat(ButtonTexts(view)));
        window.Close();
    }

    private static IEnumerable<string> ButtonTexts(Avalonia.Visual root)
        => Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(root)
            .OfType<Avalonia.Controls.Button>()
            .Select(b => b.Content?.ToString() ?? string.Empty);
}
