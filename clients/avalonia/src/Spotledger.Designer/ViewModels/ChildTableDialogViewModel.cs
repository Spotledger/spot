namespace Spotledger.Client.Designer;

/// <summary>
/// The small dialog that adds a child table: what the table field is called, the child DocType
/// it points at and that child's title. The names follow the designer's naming rules as the user
/// types, and the server still validates every one of them.
/// </summary>
public sealed class ChildTableDialogViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private DesignerPlacement? _placement;
    private bool _isOpen;
    private string _title = "Child table";
    private string _childDoctype = "child_table";
    private string _childLabel = "Child table";

    /// <summary>Creates the dialog over a session.</summary>
    public ChildTableDialogViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        CreateCommand = new AsyncRelayCommand(_ => CreateAsync(), _ => CanCreate);
        CancelCommand = new RelayCommand(_ => Close());
    }

    /// <summary>Adds the child table to the draft.</summary>
    public AsyncRelayCommand CreateCommand { get; }

    /// <summary>Closes the dialog without adding anything.</summary>
    public RelayCommand CancelCommand { get; }

    /// <summary>Raised after the dialog closed, with what the server did.</summary>
    public event EventHandler<DesignerResult>? Closed;

    /// <summary>Whether the dialog is showing.</summary>
    public bool IsOpen
    {
        get => _isOpen;
        private set => SetField(ref _isOpen, value);
    }

    /// <summary>The table field's label, as the user types it.</summary>
    public string Title
    {
        get => _title;
        set
        {
            if (SetField(ref _title, value))
            {
                OnPropertyChanged(nameof(CanCreate));
            }
        }
    }

    /// <summary>The child DocType's storage name.</summary>
    public string ChildDoctype
    {
        get => _childDoctype;
        set
        {
            if (SetField(ref _childDoctype, value))
            {
                OnPropertyChanged(nameof(CanCreate));
                OnPropertyChanged(nameof(NameHint));
            }
        }
    }

    /// <summary>The child DocType's title.</summary>
    public string ChildLabel
    {
        get => _childLabel;
        set => SetField(ref _childLabel, value);
    }

    /// <summary>A shape hint for the child name (the server is the authority).</summary>
    public string NameHint => DesignerNaming.IsStorageName(ChildDoctype)
        ? "The grid's rows are stored as this DocType."
        : "Use lower-case letters, digits and underscores, starting with a letter.";

    /// <summary>Whether the dialog has enough to add the table.</summary>
    public bool CanCreate => !string.IsNullOrWhiteSpace(Title) && DesignerNaming.IsStorageName(ChildDoctype);

    /// <summary>Opens the dialog for a placement, suggesting names from the label.</summary>
    public void Open(DesignerPlacement? placement, string? suggestedTitle = null)
    {
        _placement = placement;
        Title = string.IsNullOrWhiteSpace(suggestedTitle) ? "Child table" : suggestedTitle.Trim();
        var childName = DesignerNaming.Unique(
            _session.Draft?.ChildList.Select(c => c.Doctype).ToList() ?? new List<string>(),
            DesignerNaming.SafeIdentifier(Title));
        ChildDoctype = childName;
        ChildLabel = TitleCase(childName);
        IsOpen = true;
        OnPropertyChanged(nameof(CanCreate));
        CreateCommand.RaiseCanExecuteChanged();
    }

    /// <summary>Closes the dialog.</summary>
    public void Close()
    {
        IsOpen = false;
        Closed?.Invoke(this, DesignerResult.Success);
    }

    /// <summary>Adds the child table and its grid section, then closes.</summary>
    public async Task CreateAsync()
    {
        if (!CanCreate)
        {
            return;
        }

        var result = await _session.AddChildTableAsync(Title, ChildDoctype, ChildLabel, _placement).ConfigureAwait(true);
        if (result.Ok)
        {
            IsOpen = false;
        }

        Closed?.Invoke(this, result);
    }

    private static string TitleCase(string name)
    {
        var words = name.Split('_', StringSplitOptions.RemoveEmptyEntries);
        return words.Length == 0 ? name : string.Join(' ', words.Select(w => char.ToUpperInvariant(w[0]) + w[1..]));
    }
}
