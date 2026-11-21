using System.Globalization;
using Spotledger.Client.Expressions;

namespace Spotledger.Client.Extensions;

/// <summary>
/// A live action: the label, its slot, whether it is visible and enabled right now, its
/// busy state, and what a click sends. All of the decision-making is here, renderer-neutral,
/// so a headless test can click it without a window and a second renderer reuses it.
///
/// One click makes one <see cref="ActionRequest" /> with one idempotency key. While the
/// dispatch is in flight the action is <see cref="IsBusy" /> and a second click does
/// nothing — no double submit. The answer is applied through <see cref="IShellServices" />;
/// a failure shows the server's problem text once, never an exception.
/// </summary>
public sealed class ActionModel : SlotEntry
{
    private readonly ActionSpec _spec;
    private readonly ExtensionContext _context;
    private readonly IActionDispatcher? _dispatcher;
    private readonly IShellServices? _shell;
    private readonly DiagnosticSink _sink;
    private readonly Func<string> _keyFactory;
    private int _busy;
    private bool _isEnabled;

    internal ActionModel(
        ActionSpec spec,
        ExtensionContext context,
        ExtensionServices services,
        DiagnosticSink sink,
        Func<string>? keyFactory = null)
        : base(spec.Id, spec.App, spec.Order)
    {
        _spec = spec;
        _context = context;
        _dispatcher = services.Actions;
        _shell = services.Shell;
        _sink = sink;
        _keyFactory = keyFactory ?? (static () => Guid.NewGuid().ToString("N", CultureInfo.InvariantCulture));
        Refresh();
    }

    /// <summary>The label as the document wrote it (a literal or a <c>key:</c> translation key).</summary>
    public string Label => _spec.Label;

    /// <summary>The icon token, or null.</summary>
    public string? Icon => _spec.Icon;

    /// <summary>The text to confirm before the send, or null.</summary>
    public string? ConfirmText => _spec.Confirm;

    /// <summary>The action's effect kind.</summary>
    public ActionEffectKind EffectKind => _spec.Effect switch
    {
        CallEffect => ActionEffectKind.Call,
        OpenEffect => ActionEffectKind.Open,
        _ => ActionEffectKind.Navigate
    };

    /// <summary>False when <c>enabled_if</c> is false, the expression fails, or a dispatch is in flight.</summary>
    public bool IsEnabled => _isEnabled && !IsBusy;

    /// <summary>True while the click's request is in flight; a busy action cannot be invoked again.</summary>
    public bool IsBusy => Volatile.Read(ref _busy) == 1;

    /// <summary>The specification this model was built from.</summary>
    public ActionSpec Spec => _spec;

    /// <summary>
    /// Invokes the action once. Cancelling the token cancels the dispatch; the busy flag is
    /// always cleared, so a cancelled click does not lock the action.
    /// </summary>
    public async Task InvokeAsync(CancellationToken cancellationToken)
    {
        if (!IsVisible || !IsEnabled || Interlocked.CompareExchange(ref _busy, 1, 0) != 0)
        {
            return;
        }
        RaiseChanged();
        try
        {
            if (_dispatcher is null || _shell is null)
            {
                _sink.Report(Id, ExtensionCodes.DispatchFailed, "the host wired no action dispatcher");
                return;
            }
            if (ConfirmText is not null &&
                !await _shell.ConfirmAsync(ConfirmText, cancellationToken).ConfigureAwait(false))
            {
                return;
            }
            ActionRequest request;
            try
            {
                request = BuildRequest();
            }
            catch (Exception error) when (error is not OutOfMemoryException)
            {
                _sink.Report(Id, ExtensionCodes.DispatchFailed, error.Message);
                await _shell.ShowMessageAsync(error.Message, cancellationToken).ConfigureAwait(false);
                return;
            }
            ActionOutcome outcome;
            try
            {
                outcome = await _dispatcher.DispatchAsync(request, cancellationToken).ConfigureAwait(false);
            }
            catch (OperationCanceledException) when (cancellationToken.IsCancellationRequested)
            {
                return;
            }
            catch (Exception error) when (error is not OutOfMemoryException)
            {
                _sink.Report(Id, ExtensionCodes.DispatchFailed, error.Message);
                await _shell.ShowMessageAsync(error.Message, cancellationToken).ConfigureAwait(false);
                return;
            }
            await ApplyAsync(outcome, cancellationToken).ConfigureAwait(false);
        }
        finally
        {
            Interlocked.Exchange(ref _busy, 0);
            RaiseChanged();
        }
    }

    /// <summary>The request one click sends, built from the context the action was evaluated in.</summary>
    public ActionRequest BuildRequest()
    {
        string key = _keyFactory();
        switch (_spec.Effect)
        {
            case CallEffect call:
                {
                    List<KeyValuePair<string, ExpressionValue>> arguments = Evaluate(call.Arguments);
                    if (_spec.Slot.StartsWith("list.", StringComparison.Ordinal) &&
                        !arguments.Exists(static pair => string.Equals(pair.Key, "ids", StringComparison.Ordinal)))
                    {
                        arguments.AddRange(_context.SelectionArguments());
                    }
                    bool isCurrentDocument = string.Equals(call.Doctype, _context.Draft?.Model.Name, StringComparison.Ordinal);
                    return new ActionRequest(Id, App, ActionEffectKind.Call, Doctype: call.Doctype, Method: call.Method,
                        Arguments: arguments,
                        DocumentId: _context.DocumentId,
                        DocumentVersion: isCurrentDocument ? _context.DocumentVersion : null,
                        IdempotencyKey: key);
                }
            case OpenEffect open:
                return new ActionRequest(Id, App, ActionEffectKind.Open, Destination: open.Destination,
                    Filters: Evaluate(open.Filters), IdempotencyKey: key);
            case NavigateEffect navigate:
                return new ActionRequest(Id, App, ActionEffectKind.Navigate, Path: navigate.Path, IdempotencyKey: key);
            default:
                throw new InvalidOperationException("an action has exactly one effect");
        }
    }

    /// <summary>Re-evaluates visibility and enabled state; called when the context changes.</summary>
    internal void Refresh()
    {
        bool visible = _context.HasPermission(_spec.Permission);
        if (visible && _spec.VisibleIf is not null)
        {
            visible = _spec.VisibleIf.EvaluateBool(_context, _sink, Id, fallback: false);
        }
        IsVisible = visible;
        bool enabled = visible;
        if (enabled && _spec.EnabledIf is not null)
        {
            enabled = _spec.EnabledIf.EvaluateBool(_context, _sink, Id, fallback: false);
        }
        if (_isEnabled != enabled)
        {
            _isEnabled = enabled;
            RaiseChanged();
        }
    }

    private List<KeyValuePair<string, ExpressionValue>> Evaluate(IReadOnlyList<KeyValuePair<string, CompiledExpression>> expressions)
    {
        List<KeyValuePair<string, ExpressionValue>> values = new(expressions.Count);
        foreach (KeyValuePair<string, CompiledExpression> expression in expressions)
        {
            values.Add(new(expression.Key, expression.Value.Evaluate(_context, _sink, Id)));
        }
        return values;
    }

    private async Task ApplyAsync(ActionOutcome outcome, CancellationToken cancellationToken)
    {
        if (_shell is null)
        {
            return;
        }
        switch (outcome.Kind)
        {
            case ActionOutcomeKind.Refresh:
                await _shell.RefreshAsync(_spec.Doctype, _context.DocumentId, cancellationToken).ConfigureAwait(false);
                break;
            case ActionOutcomeKind.Open when outcome.Destination is { } destination:
                await _shell.OpenAsync(destination, outcome.Filters ?? [], cancellationToken).ConfigureAwait(false);
                break;
            case ActionOutcomeKind.Open:
                _sink.Report(Id, ExtensionCodes.DispatchFailed, "the answer opened no destination");
                break;
            case ActionOutcomeKind.ShowMessage:
                await _shell.ShowMessageAsync(outcome.Message ?? string.Empty, cancellationToken).ConfigureAwait(false);
                break;
            case ActionOutcomeKind.Download when outcome.Reference is { } reference:
                await _shell.DownloadAsync(reference, cancellationToken).ConfigureAwait(false);
                break;
            case ActionOutcomeKind.Download:
                _sink.Report(Id, ExtensionCodes.DispatchFailed, "the answer named no download");
                break;
            case ActionOutcomeKind.Failed:
                {
                    string problem = outcome.Problem ?? outcome.Message ?? "The action failed.";
                    _sink.Report(Id, ExtensionCodes.DispatchFailed, problem);
                    await _shell.ShowMessageAsync(problem, cancellationToken).ConfigureAwait(false);
                    break;
                }
            default:
                break;
        }
    }

    /// <inheritdoc/>
    public override string ToString() => $"{Id} ({Label})";
}
