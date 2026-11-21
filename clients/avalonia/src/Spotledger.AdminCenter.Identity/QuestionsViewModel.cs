using Spotledger.Client.Shell;

namespace Spotledger.AdminCenter.Identity;

/// <summary>
/// The four answerable questions on one page, one tab each: what can this user do, who can do this,
/// who can see this document and what changed between two dates. Every row can expand its why-chain.
/// Export is offered only to a caller with the export permission, and the server checks it again.
/// </summary>
public sealed class QuestionsViewModel : IdentityViewModel
{
    private QuestionKind _tab = QuestionKind.WhatCanUserDo;
    private AnswerPage? _answer;
    private QuestionRequest? _lastAsked;
    private readonly HashSet<string> _expanded = [];

    /// <summary>Creates the page. <paramref name="canExport"/> is the caller's export permission.</summary>
    public QuestionsViewModel(IIdentityGateway gateway, IIdentityShell shell, bool canExport)
        : base(gateway, shell)
    {
        CanExport = canExport;
    }

    /// <summary>The four tabs, in order.</summary>
    public static IReadOnlyList<QuestionKind> Tabs { get; } =
        [QuestionKind.WhatCanUserDo, QuestionKind.WhoCanDo, QuestionKind.WhoCanSee, QuestionKind.WhatChanged];

    /// <summary>The selected tab. Changing it clears the answer, which belongs to the other question.</summary>
    public QuestionKind Tab
    {
        get => _tab;
        set
        {
            if (SetField(ref _tab, value))
            {
                Answer = null;
                _lastAsked = null;
                _expanded.Clear();
                Error = null;
            }
        }
    }

    /// <summary>Whether the caller may export.</summary>
    public bool CanExport { get; }

    /// <summary>The current answer.</summary>
    public AnswerPage? Answer
    {
        get => _answer;
        private set
        {
            if (SetField(ref _answer, value))
            {
                OnPropertyChanged(nameof(ExportAvailable));
            }
        }
    }

    /// <summary>True when there is an answer and the caller may export it.</summary>
    public bool ExportAvailable => CanExport && Answer is not null;

    /// <summary>Asks the question on the current tab. The request's kind is forced to the tab.</summary>
    public async Task<bool> AskAsync(QuestionRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        var asked = request with { Kind = Tab, Cursor = null };
        var missing = Missing(asked);
        if (missing is not null)
        {
            Error = missing;
            return false;
        }

        var page = await CallAsync(() => Gateway.AskAsync(asked));
        if (page is null)
        {
            // Includes "you may not ask this": the server's message is shown, no rows.
            Answer = null;
            return false;
        }

        _lastAsked = asked;
        _expanded.Clear();
        Answer = page;
        Status = page.Rows.Count == 0 ? "No results." : $"{page.Rows.Count} result(s).";
        return true;
    }

    private static string? Missing(QuestionRequest request) => request.Kind switch
    {
        QuestionKind.WhatCanUserDo when string.IsNullOrWhiteSpace(request.UserId) => "Pick a user.",
        QuestionKind.WhoCanDo when string.IsNullOrWhiteSpace(request.Permission) => "Pick a permission.",
        QuestionKind.WhoCanSee when string.IsNullOrWhiteSpace(request.Doctype) || string.IsNullOrWhiteSpace(request.DocumentId) => "Pick a document.",
        QuestionKind.WhatChanged when request.From is null || request.To is null => "Pick both dates.",
        QuestionKind.WhatChanged when request.To < request.From => "The end date is before the start date.",
        _ => null,
    };

    /// <summary>Loads the next page and appends it.</summary>
    public async Task<bool> MoreAsync()
    {
        if (_lastAsked is null || Answer?.NextCursor is not { } cursor)
        {
            return false;
        }

        var next = await CallAsync(() => Gateway.AskAsync(_lastAsked with { Cursor = cursor }));
        if (next is null)
        {
            return false;
        }

        Answer = new AnswerPage([.. Answer.Rows, .. next.Rows], next.NextCursor);
        return true;
    }

    /// <summary>Whether a row's why-chain is open.</summary>
    public bool IsExpanded(AnswerRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        return _expanded.Contains(row.Subject + "\u001f" + row.Summary);
    }

    /// <summary>Opens or closes a row's why-chain. A row with no chain stays closed.</summary>
    public void ToggleWhy(AnswerRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        if (row.Why.Count == 0)
        {
            return;
        }

        var key = row.Subject + "\u001f" + row.Summary;
        if (!_expanded.Remove(key))
        {
            _expanded.Add(key);
        }

        OnPropertyChanged(nameof(Answer));
    }

    /// <summary>Exports the current answer. Refused without the permission, before anything is asked of the server.</summary>
    public async Task<string?> ExportAsync()
    {
        if (!CanExport)
        {
            Error = "You do not have permission to export this answer.";
            return null;
        }

        if (_lastAsked is null)
        {
            Error = "Ask a question first.";
            return null;
        }

        return await CallAsync(() => Gateway.ExportAnswerAsync(_lastAsked));
    }
}
