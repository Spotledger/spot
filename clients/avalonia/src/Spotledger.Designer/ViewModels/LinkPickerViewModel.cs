using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>
/// The link picker: search the site's DocTypes by name and pick the one a link field points at.
/// The results come from the server, so a designer can only link to a DocType that exists.
/// </summary>
public sealed class LinkPickerViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private string _query = string.Empty;
    private string _fieldLabel = "Link";
    private bool _isOpen;
    private DesignerPlacement? _placement;
    private DocTypeChoice? _selected;

    /// <summary>Creates the picker over a session.</summary>
    public LinkPickerViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        SearchCommand = new AsyncRelayCommand(_ => SearchAsync());
        ChooseCommand = new AsyncRelayCommand(p => ChooseAsync(p as DocTypeChoice));
        CancelCommand = new RelayCommand(_ => Close());
    }

    /// <summary>Runs the search.</summary>
    public AsyncRelayCommand SearchCommand { get; }

    /// <summary>Picks the DocType in the parameter (or the selected row).</summary>
    public AsyncRelayCommand ChooseCommand { get; }

    /// <summary>Closes the picker without choosing.</summary>
    public RelayCommand CancelCommand { get; }

    /// <summary>Raised after a DocType was chosen and the link field added.</summary>
    public event EventHandler<DesignerResult>? Chosen;

    /// <summary>Whether the picker is showing.</summary>
    public bool IsOpen
    {
        get => _isOpen;
        private set => SetField(ref _isOpen, value);
    }

    /// <summary>The search text.</summary>
    public string Query
    {
        get => _query;
        set => SetField(ref _query, value);
    }

    /// <summary>The search results.</summary>
    public ObservableCollection<DocTypeChoice> Results { get; } = new();

    /// <summary>The result the user picked (the view's selection, and Choose's default).</summary>
    public DocTypeChoice? Selected
    {
        get => _selected;
        set => SetField(ref _selected, value);
    }

    /// <summary>What the picker is for, in the dialog's words.</summary>
    public string Prompt { get; private set; } = "Choose the DocType this field links to.";

    /// <summary>How the new link field is labelled; the user may change it before choosing.</summary>
    public string FieldLabel
    {
        get => _fieldLabel;
        set => SetField(ref _fieldLabel, value);
    }

    /// <summary>Whether the chosen DocType may be a child table (the child-table picker).</summary>
    public bool AllowChildren { get; private set; }

    /// <summary>Opens the picker for a placement and seed query.</summary>
    public void Open(string fieldLabel, DesignerPlacement? placement, string? query = null, bool allowChildren = false)
    {
        FieldLabel = string.IsNullOrWhiteSpace(fieldLabel) ? "Link" : fieldLabel;
        _placement = placement;
        AllowChildren = allowChildren;
        Prompt = allowChildren ? "Choose an existing DocType to hold the rows." : "Choose the DocType this field links to.";
        Query = query ?? string.Empty;
        IsOpen = true;
        Results.Clear();
        Selected = null;
    }

    /// <summary>Closes the picker.</summary>
    public void Close() => IsOpen = false;

    /// <summary>Searches the server for DocTypes matching the query.</summary>
    public async Task SearchAsync()
    {
        var found = await _session.SearchDocTypesAsync(Query).ConfigureAwait(true);
        Results.Clear();
        foreach (var choice in found.Where(c => AllowChildren || !c.IsChild))
        {
            Results.Add(choice);
        }

        Selected = Results.FirstOrDefault();
    }

    /// <summary>Adds the link (or child table) field pointing at the chosen DocType.</summary>
    public async Task ChooseAsync(DocTypeChoice? choice)
    {
        choice ??= Selected;
        if (choice is null)
        {
            return;
        }

        var result = await _session.AddLinkAsync(FieldLabel, choice.Name, _placement).ConfigureAwait(true);
        Apply(result);
    }

    /// <summary>Adds a child table whose rows are the chosen DocType.</summary>
    public async Task ChooseChildAsync(DocTypeChoice? choice)
    {
        choice ??= Selected;
        if (choice is null)
        {
            return;
        }

        var result = await _session.AddChildTableAsync(FieldLabel, choice.Name, choice.Label, _placement).ConfigureAwait(true);
        Apply(result);
    }

    private void Apply(DesignerResult result)
    {
        if (result.Ok)
        {
            IsOpen = false;
        }

        Chosen?.Invoke(this, result);
    }
}
