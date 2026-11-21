using Avalonia.Input;

namespace Spotledger.Client.Workspace.Tests.Workflows;

/// <summary>
/// One step of a keyboard script. There is deliberately no pointer step: a workflow that needs a click
/// cannot be written, which is the point of the harness.
/// </summary>
public abstract record KeyStep(string Why);

/// <summary>A key press with its modifiers.</summary>
public sealed record Press(Key Key, string Why, KeyModifiers Modifiers = KeyModifiers.None) : KeyStep(Why);

/// <summary>Text typed into the focused editor (the platform's text input event).</summary>
public sealed record Type(string Text, string Why) : KeyStep(Why);

/// <summary>The focus must be on the control with this automation id.</summary>
public sealed record ExpectFocus(string AutomationId, string Why) : KeyStep(Why);

/// <summary>The control with this automation id must show this text.</summary>
public sealed record ExpectText(string AutomationId, string Text, string Why) : KeyStep(Why);

/// <summary>A screen-reader announcement must have said this (since the script started).</summary>
public sealed record ExpectAnnouncement(string Contains, string Why) : KeyStep(Why);

/// <summary>The application state must satisfy this now.</summary>
public sealed record Expect(Func<bool> Condition, string Why) : KeyStep(Why);

/// <summary>
/// The state must satisfy this after some pumping. This is how a workflow waits: it pumps the
/// dispatcher and reads the state it is waiting for. It never sleeps, so a slow machine makes the wait
/// longer, not the test flaky.
/// </summary>
public sealed record Await(Func<bool> Condition, string Why, int Attempts = 2000) : KeyStep(Why);

/// <summary>A named script: the steps, a keystroke budget and a time budget.</summary>
/// <param name="Name">The workflow's name (the test asserts on it).</param>
/// <param name="Steps">The steps, in order.</param>
/// <param name="Keystrokes">The most keystrokes the workflow may need; a workflow that suddenly needs
/// hundreds has grown a wrong turn and fails.</param>
/// <param name="Time">The most wall-clock time the workflow may take.</param>
public sealed record KeyScript(string Name, IReadOnlyList<KeyStep> Steps, int Keystrokes, TimeSpan Time);

/// <summary>What a workflow cost, for the test's assertions and the report.</summary>
/// <param name="Name">The workflow.</param>
/// <param name="Keystrokes">Keystrokes actually sent (a typed character counts as one).</param>
/// <param name="Elapsed">Wall-clock time.</param>
/// <param name="PointerEvents">Pointer events observed: must be zero.</param>
public sealed record WorkflowResult(string Name, int Keystrokes, TimeSpan Elapsed, int PointerEvents);

/// <summary>A workflow that could not finish, with the step that failed.</summary>
public sealed class WorkflowFailedException : Exception
{
    /// <summary>Creates the failure.</summary>
    public WorkflowFailedException(string workflow, int step, KeyStep failing, string message, Exception? inner = null)
        : base($"Workflow '{workflow}' failed at step {step} ({failing.GetType().Name}: {failing.Why}): {message}", inner)
    {
        Workflow = workflow;
        Step = step;
    }

    /// <summary>The workflow's name.</summary>
    public string Workflow { get; }

    /// <summary>The zero-based index of the failing step.</summary>
    public int Step { get; }
}
