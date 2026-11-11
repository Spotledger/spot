using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Metadata;
using Spotledger.Client.Workspace.Split;
using SplitRig = Spotledger.Client.Workspace.Tests.Split.SplitRig;
using Xunit;

namespace Spotledger.Client.Workspace.Tests.Workflows;

/// <summary>
/// The first two scripted workflows of the sample app, run through the real headless views with key
/// events only: no pointer step exists in the harness, and the runner fails if a pointer event happens
/// anyway. Each script declares its keystroke budget and its time budget; a workflow that grows a wrong
/// turn fails instead of quietly costing more.
/// </summary>
public sealed class SplitWorkflows
{
    [AvaloniaFact]
    public async Task Open_a_document_edit_two_fields_and_save()
    {
        using var rig = new SplitRig(rows: 3);
        var runner = new WorkflowRunner(rig.Window, rig.Trace);
        var before = Title(rig);

        var script = new KeyScript(
            "edit-two-fields-save",
            [
                new Press(Key.L, "move focus to the list", KeyModifiers.Control | KeyModifiers.Alt),
                new Press(Key.Down, "select the first row of the list", KeyModifiers.Alt),
                new Await(() => rig.Split.CurrentDocumentId == "T-1", "the first document to open"),
                new ExpectFocus("field:title", "the form opens on its first field"),
                new Type(" renamed", "edit the title"),
                new Press(Key.Tab, "commit the title and move to the field's clear button"),
                new ExpectFocus("field:title", "the clear button inside the title field"),
                new Press(Key.Tab, "move on to the next field"),
                new ExpectFocus("field:done", "Tab reaches the done box"),
                new Press(Key.Space, "change the second field"),
                new Press(Key.Tab, "commit the tick and leave the box"),
                new Expect(() => Done(rig), "the done box is ticked"),
                new Press(Key.S, "save", KeyModifiers.Control),
                new Await(() => rig.DocumentGateway.Patches.Count == 1, "the save to reach the server"),
                // The form's own Ctrl+S keeps the focus where it was; the arrangement only announces a
                // save it performed itself (save-and-next), which the worklist workflow exercises.
                new ExpectFocus("field:items", "the save left the focus in the form"),
                new Expect(() => Title(rig).Contains("renamed", StringComparison.Ordinal), "the typed title was sent"),
                new Expect(() => Done(rig), "the ticked box was sent"),
            ],
            Keystrokes: 40,
            Time: TimeSpan.FromSeconds(25));

        var result = runner.Run(script);

        Assert.Equal(0, result.PointerEvents);
        Assert.InRange(result.Keystrokes, 1, 40);
        Assert.NotEqual(before, Title(rig));
        Assert.All(rig.DocumentGateway.Patches, patch => Assert.Contains("renamed", patch, StringComparison.Ordinal));
        runner.Detach();
    }

    [AvaloniaFact]
    public async Task Walk_a_worklist_of_fifteen_tasks_and_close_each_one_with_save_and_next()
    {
        using var rig = new SplitRig(rows: 15);
        var runner = new WorkflowRunner(rig.Window, rig.Trace);

        var steps = new List<KeyStep>
        {
            new Press(Key.L, "move focus to the list", KeyModifiers.Control | KeyModifiers.Alt),
            new Press(Key.Down, "select the first task", KeyModifiers.Alt),
            new Await(() => rig.Split.CurrentDocumentId == "T-1", "the first task to open"),
        };

        for (var index = 1; index <= 15; index++)
        {
            var expected = "T-" + index.ToString(System.Globalization.CultureInfo.InvariantCulture);
            steps.Add(new Press(Key.Tab, "commit the title of " + expected));
            steps.Add(new Press(Key.Tab, "move to the done box of " + expected));
            steps.Add(new Press(Key.Space, "close " + expected));
            steps.Add(new Press(Key.S, "save and move to the next task", KeyModifiers.Control | KeyModifiers.Alt));
            steps.Add(new Await(
                () => rig.ListGateway.Rows.Find(row => row.Id == expected) is { Done: true },
                expected + " to be saved as done"));
        }

        steps.Add(new Expect(() => rig.Split.CurrentDocumentId == "T-15", "the walk ends on the last task"));
        steps.Add(new Expect(() => rig.ListGateway.Rows.TrueForAll(row => row.Done), "every task is closed"));

        var script = new KeyScript("walk-and-close-15", steps, Keystrokes: 80, Time: TimeSpan.FromSeconds(40));
        var result = runner.Run(script);

        Assert.Equal(0, result.PointerEvents);
        Assert.Equal(15, rig.DocumentGateway.Patches.Count);
        Assert.All(rig.ListGateway.Rows, row => Assert.True(row.Done, row.Id + " is closed"));
        runner.Detach();
    }

    private static string Title(SplitRig rig)
        => rig.Split.Form?.Model.Draft?.Get("title") is TextFieldValue text ? text.Value : string.Empty;

    private static bool Done(SplitRig rig)
        => rig.Split.Form?.Model.Draft?.Get("done") is BoolFieldValue flag && flag.Value;
}
