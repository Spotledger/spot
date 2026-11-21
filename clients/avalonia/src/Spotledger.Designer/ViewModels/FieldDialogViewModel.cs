namespace Spotledger.Client.Designer;

/// <summary>
/// The small dialog a palette drop opens: the field's label and storage name, before the field is
/// added. It exists because the server's <c>add_field</c> op takes the label and the draft model has
/// no change-the-label op: a field has to be named as it is created, or it keeps the palette's
/// generic word forever.
/// </summary>
public sealed class FieldDialogViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private DesignerPlacement? _placement;
    private string? _childDoctype;
    private FieldTypeInfo? _type;
    private bool _isOpen;
    private string _label = string.Empty;
    private string _name = string.Empty;
    private bool _nameEdited;

    /// <summary>Creates the dialog over a session.</summary>
    public FieldDialogViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        CreateCommand = new AsyncRelayCommand(_ => CreateAsync(), _ => CanCreate);
        CancelCommand = new RelayCommand(_ => Close());
    }

    /// <summary>Adds the field to the draft.</summary>
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

    /// <summary>The type being added, as the palette names it.</summary>
    public string TypeTitle => _type?.Title ?? string.Empty;

    /// <summary>What the type is for, in the palette's words.</summary>
    public string TypeDescription => _type?.Description ?? string.Empty;

    /// <summary>The field's label, as the user types it.</summary>
    public string Label
    {
        get => _label;
        set
        {
            if (!SetField(ref _label, value))
            {
                return;
            }

            if (!_nameEdited)
            {
                Name = SuggestName(value);
            }

            OnPropertyChanged(nameof(CanCreate));
            CreateCommand.RaiseCanExecuteChanged();
        }
    }

    /// <summary>The field's storage name; editable, and derived from the label until it is edited.</summary>
    public string Name
    {
        get => _name;
        set
        {
            if (SetField(ref _name, value))
            {
                _nameEdited = !string.Equals(value, SuggestName(Label), StringComparison.Ordinal);
                OnPropertyChanged(nameof(CanCreate));
                OnPropertyChanged(nameof(NameHint));
                CreateCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>Whether the dialog has enough to add the field.</summary>
    public bool CanCreate => !string.IsNullOrWhiteSpace(Label) && DesignerNaming.IsStorageName(Name);

    /// <summary>A shape hint for the name (the server is the authority).</summary>
    public string NameHint => DesignerNaming.IsStorageName(Name)
        ? "Every API call and report uses this name; the label is what people see."
        : "Use lower-case letters, digits and underscores, starting with a letter.";

    /// <summary>Opens the dialog for a palette type and a place on the form.</summary>
    public void Open(FieldTypeInfo type, DesignerPlacement? placement, string? childDoctype = null)
    {
        ArgumentNullException.ThrowIfNull(type);
        _type = type;
        _placement = placement;
        _childDoctype = childDoctype;
        _nameEdited = false;
        _label = type.Title;
        _name = SuggestName(type.Title);
        IsOpen = true;
        OnPropertyChanged(nameof(Label));
        OnPropertyChanged(nameof(Name));
        OnPropertyChanged(nameof(TypeTitle));
        OnPropertyChanged(nameof(TypeDescription));
        OnPropertyChanged(nameof(NameHint));
        OnPropertyChanged(nameof(CanCreate));
        CreateCommand.RaiseCanExecuteChanged();
    }

    /// <summary>Closes the dialog.</summary>
    public void Close()
    {
        IsOpen = false;
        Closed?.Invoke(this, DesignerResult.Success);
    }

    /// <summary>Adds the field with the label and name the user chose.</summary>
    public async Task CreateAsync()
    {
        if (!CanCreate || _type is null)
        {
            return;
        }

        var result = await _session.AddFieldAsync(Label, _type.Type, _placement, _childDoctype, Name).ConfigureAwait(true);
        if (result.Ok)
        {
            IsOpen = false;
        }
        else
        {
            OnPropertyChanged(nameof(NameHint));
        }

        Closed?.Invoke(this, result);
    }

    private string SuggestName(string label)
    {
        var taken = _session.Draft?.FieldsOf(_childDoctype).Select(f => f.Name).ToList() ?? new List<string>();
        var baseName = DesignerNaming.FieldName(label, !(_session.Draft?.NewDocType ?? true));
        return DesignerNaming.Unique(taken, baseName);
    }
}
