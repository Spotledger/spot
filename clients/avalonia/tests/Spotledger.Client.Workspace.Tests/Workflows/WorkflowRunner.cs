using System.Diagnostics;
using System.Globalization;
using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Headless;
using Avalonia.Input;
using Avalonia.VisualTree;
using Spotledger.Client.Workspace.Keyboard;

namespace Spotledger.Client.Workspace.Tests.Workflows;

/// <summary>
/// Runs a <see cref="KeyScript"/> through the real window: every step is a key event, a text input event
/// or an assertion about the state. It counts keystrokes and wall-clock time against the script's
/// budgets, and it counts pointer events — a workflow that needed one fails instead of passing quietly.
/// </summary>
public sealed class WorkflowRunner
{
    private readonly Window _window;
    private readonly FocusTrace _trace;
    private int _pointerEvents;

    /// <summary>Creates a runner over the window the views live in.</summary>
    public WorkflowRunner(Window window, FocusTrace trace)
    {
        _window = window ?? throw new ArgumentNullException(nameof(window));
        _trace = trace ?? throw new ArgumentNullException(nameof(trace));
        _window.PointerPressed += OnPointer;
        _window.PointerReleased += OnPointer;
        _window.PointerMoved += OnPointer;
    }

    /// <summary>Runs the script and asserts its budgets.</summary>
    public WorkflowResult Run(KeyScript script)
    {
        ArgumentNullException.ThrowIfNull(script);
        var keystrokes = 0;
        var startEntries = _trace.Entries.Count;
        var watch = Stopwatch.StartNew();
        for (var index = 0; index < script.Steps.Count; index++)
        {
            var step = script.Steps[index];
            try
            {
                keystrokes += Execute(step, startEntries);
            }
            catch (WorkflowFailedException)
            {
                throw;
            }
            catch (Exception e) when (e is InvalidOperationException or Xunit.Sdk.XunitException or TimeoutException)
            {
                throw new WorkflowFailedException(script.Name, index, step, e.Message, e);
            }
        }

        watch.Stop();
        if (_pointerEvents > 0)
        {
            throw new WorkflowFailedException(
                script.Name,
                script.Steps.Count - 1,
                script.Steps[^1],
                $"the workflow used pointer input ({_pointerEvents} events); a workflow must be keyboard-only");
        }

        if (keystrokes > script.Keystrokes)
        {
            throw new WorkflowFailedException(
                script.Name,
                script.Steps.Count - 1,
                script.Steps[^1],
                string.Create(CultureInfo.InvariantCulture, $"the workflow needed {keystrokes} keystrokes, over its budget of {script.Keystrokes}"));
        }

        if (watch.Elapsed > script.Time)
        {
            throw new WorkflowFailedException(
                script.Name,
                script.Steps.Count - 1,
                script.Steps[^1],
                string.Create(CultureInfo.InvariantCulture, $"the workflow took {watch.Elapsed.TotalSeconds:F1}s, over its budget of {script.Time.TotalSeconds:F1}s"));
        }

        return new WorkflowResult(script.Name, keystrokes, watch.Elapsed, _pointerEvents);
    }

    /// <summary>Stops counting pointer events (the runner is single-use in practice; a test may reuse it).</summary>
    public void Detach()
    {
        _window.PointerPressed -= OnPointer;
        _window.PointerReleased -= OnPointer;
        _window.PointerMoved -= OnPointer;
    }

    private int Execute(KeyStep step, int startEntries)
    {
        switch (step)
        {
            case Press press:
                _window.KeyPress(press.Key, Raw(press.Modifiers), PhysicalKey.None, null);
                _window.KeyRelease(press.Key, Raw(press.Modifiers), PhysicalKey.None, null);
                TestHost.Pump();
                return 1;

            case Type type:
                _window.KeyTextInput(type.Text);
                TestHost.Pump();
                return type.Text.Length;

            case ExpectFocus focus:
                if (!string.Equals(_trace.Current, focus.AutomationId, StringComparison.Ordinal))
                {
                    throw new InvalidOperationException(
                        $"expected the focus on '{focus.AutomationId}' but it is on '{_trace.Current ?? "nothing"}'");
                }

                return 0;

            case ExpectText text:
                var actual = TextOf(text.AutomationId);
                if (!string.Equals(actual, text.Text, StringComparison.Ordinal))
                {
                    throw new InvalidOperationException($"expected '{text.Text}' in '{text.AutomationId}' but found '{actual}'");
                }

                return 0;

            case ExpectAnnouncement announcement:
                var said = _trace.Entries.Skip(startEntries)
                    .Where(entry => entry.Kind == FocusKind.Announcement)
                    .Select(entry => entry.Note)
                    .ToList();
                if (!said.Any(text => text.Contains(announcement.Contains, StringComparison.OrdinalIgnoreCase)))
                {
                    throw new InvalidOperationException(
                        $"expected an announcement containing '{announcement.Contains}' but heard: {string.Join(" | ", said)}");
                }

                return 0;

            case Expect expect:
                if (!expect.Condition())
                {
                    throw new InvalidOperationException("the expected state did not hold");
                }

                return 0;

            case Await awaitStep:
                Split.SplitRig.PumpUntil(awaitStep.Condition, awaitStep.Why, awaitStep.Attempts);
                return 0;

            default:
                throw new InvalidOperationException("Unknown step " + step.GetType().Name);
        }
    }

    private string TextOf(string automationId)
    {
        var control = Avalonia.VisualTree.VisualExtensions.GetVisualDescendants(_window)
            .OfType<Control>()
            .FirstOrDefault(candidate => AutomationProperties.GetAutomationId(candidate) == automationId);
        return control switch
        {
            null => throw new InvalidOperationException($"no control has the automation id '{automationId}'"),
            TextBox box => box.Text ?? string.Empty,
            Controls.SpotTextInput input => input.Text ?? string.Empty,
            TextBlock block => block.Text ?? string.Empty,
            _ => throw new InvalidOperationException($"the control '{automationId}' has no readable text"),
        };
    }

    private void OnPointer(object? sender, EventArgs e) => _pointerEvents++;

    private static RawInputModifiers Raw(KeyModifiers modifiers)
    {
        var raw = RawInputModifiers.None;
        if (modifiers.HasFlag(KeyModifiers.Shift))
        {
            raw |= RawInputModifiers.Shift;
        }

        if (modifiers.HasFlag(KeyModifiers.Control))
        {
            raw |= RawInputModifiers.Control;
        }

        if (modifiers.HasFlag(KeyModifiers.Alt))
        {
            raw |= RawInputModifiers.Alt;
        }

        return raw;
    }
}
