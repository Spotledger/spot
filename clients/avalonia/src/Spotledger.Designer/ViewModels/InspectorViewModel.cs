using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>One entry of the choice-list editor.</summary>
public sealed class OptionRow : ObservableObject
{
    private string _text;

    /// <summary>Creates a row.</summary>
    public OptionRow(string text) => _text = text;

    /// <summary>The option's text.</summary>
    public string Text
    {
        get => _text;
        set => SetField(ref _text, value);
    }
}

/// <summary>
/// The inspector for the selected field: its label and name, type, required, default, choice
/// list, condition (with the server-compiled preview), permissions template and the properties the
/// server's draft model can (and cannot) change.
/// </summary>
/// <remarks>
/// The three inspector rows the server's draft model has no op for — a label changed after the
/// field exists, read-only, hidden and a per-field validation message — are shown with their
/// current value and a hint, and their editors are disabled. They are not sent as invented ops:
/// the op catalogue in <see cref="DraftDocument.SupportedOps"/> decides. Finishing them needs the
/// server ops; see the task report's open question.
/// </remarks>
public sealed class InspectorViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private readonly IReadOnlyList<PermissionsTemplateInfo> _templates = PermissionsTemplates.All;
    private CanvasField? _field;
    private string _label = string.Empty;
    private string _name = string.Empty;
    private bool _required;
    private string _defaultValue = string.Empty;
    private string _conditionPreview = string.Empty;
    private string _conditionError = string.Empty;
    private string _selectedTemplate = "standard";
    private TypeChangePlanSummary? _typeChange;
    private string _nameError = string.Empty;

    /// <summary>Creates the inspector over a session.</summary>
    public InspectorViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        Condition = new ConditionDraft();
        ApplyNameCommand = new AsyncRelayCommand(_ => ApplyNameAsync());
        ApplyDefaultCommand = new AsyncRelayCommand(_ => ApplyDefaultAsync());
        ApplyOptionsCommand = new AsyncRelayCommand(_ => ApplyOptionsAsync());
        AddOptionCommand = new RelayCommand(_ => Options.Add(new OptionRow(string.Empty)));
        RemoveOptionCommand = new RelayCommand(p => RemoveOption(p as OptionRow));
        MoveOptionUpCommand = new RelayCommand(p => MoveOption(p as OptionRow, -1));
        MoveOptionDownCommand = new RelayCommand(p => MoveOption(p as OptionRow, 1));
        ToggleRequiredCommand = new AsyncRelayCommand(_ => ToggleRequiredAsync());
        AddConditionRowCommand = new RelayCommand(_ => AddConditionRow());
        RemoveConditionRowCommand = new RelayCommand(p => RemoveConditionRow(p));
        PreviewConditionCommand = new AsyncRelayCommand(_ => RefreshConditionAsync());
        ApplyConditionCommand = new AsyncRelayCommand(_ => ApplyConditionAsync());
        ClearConditionCommand = new AsyncRelayCommand(_ => ClearConditionAsync());
        PlanTypeChangeCommand = new AsyncRelayCommand(p => PlanTypeChangeAsync(p as string ?? "text"));
        PlanSelectedTypeChangeCommand = new AsyncRelayCommand(_ => PlanTypeChangeAsync(SelectedTypeChoice?.Type ?? "text"));
        ApplyTypeChangeCommand = new AsyncRelayCommand(_ => ApplyTypeChangeAsync());
        ApplyTemplateCommand = new AsyncRelayCommand(_ => ApplyTemplateAsync());
        ApplyNamingCommand = new AsyncRelayCommand(_ => ApplyNamingAsync());
    }

    /// <summary>The inspector's commands, bound by the view.</summary>
    public AsyncRelayCommand ApplyNameCommand { get; }

    /// <summary>Sets the default value.</summary>
    public AsyncRelayCommand ApplyDefaultCommand { get; }

    /// <summary>Saves the choice list.</summary>
    public AsyncRelayCommand ApplyOptionsCommand { get; }

    /// <summary>Adds an empty choice.</summary>
    public RelayCommand AddOptionCommand { get; }

    /// <summary>Removes a choice.</summary>
    public RelayCommand RemoveOptionCommand { get; }

    /// <summary>Moves a choice up.</summary>
    public RelayCommand MoveOptionUpCommand { get; }

    /// <summary>Moves a choice down.</summary>
    public RelayCommand MoveOptionDownCommand { get; }

    /// <summary>Marks the field required, or stops requiring it.</summary>
    public AsyncRelayCommand ToggleRequiredCommand { get; }

    /// <summary>Adds a condition row.</summary>
    public RelayCommand AddConditionRowCommand { get; }

    /// <summary>Removes the condition row in the parameter.</summary>
    public RelayCommand RemoveConditionRowCommand { get; }

    /// <summary>Asks the server to compile the rows and shows the text.</summary>
    public AsyncRelayCommand PreviewConditionCommand { get; }

    /// <summary>Saves the condition on the field.</summary>
    public AsyncRelayCommand ApplyConditionCommand { get; }

    /// <summary>Clears the field's condition.</summary>
    public AsyncRelayCommand ClearConditionCommand { get; }

    /// <summary>Asks the server to classify a type change.</summary>
    public AsyncRelayCommand PlanTypeChangeCommand { get; }

    /// <summary>Asks the server to classify the change to <see cref="SelectedTypeChoice"/>.</summary>
    public AsyncRelayCommand PlanSelectedTypeChangeCommand { get; }

    /// <summary>The types the change-type picker offers.</summary>
    public IReadOnlyList<FieldTypeInfo> TypeChoices { get; } = FieldTypeCatalog.All;

    private FieldTypeInfo? _selectedTypeChoice;

    /// <summary>The type the user picked for the selected field.</summary>
    public FieldTypeInfo? SelectedTypeChoice
    {
        get => _selectedTypeChoice;
        set => SetField(ref _selectedTypeChoice, value);
    }

    /// <summary>Applies the classified type change.</summary>
    public AsyncRelayCommand ApplyTypeChangeCommand { get; }

    /// <summary>Chooses the permissions template.</summary>
    public AsyncRelayCommand ApplyTemplateCommand { get; }

    /// <summary>Sets the DocType's naming series.</summary>
    public AsyncRelayCommand ApplyNamingCommand { get; }

    /// <summary>The selected field, or null when nothing is selected.</summary>
    public CanvasField? Field
    {
        get => _field;
        private set
        {
            if (SetField(ref _field, value))
            {
                OnPropertyChanged(nameof(HasField));
                OnPropertyChanged(nameof(TypeTitle));
                OnPropertyChanged(nameof(LabelHint));
                OnPropertyChanged(nameof(IsSelect));
            }
        }
    }

    /// <summary>Whether a field is selected.</summary>
    public bool HasField => Field is not null;

    /// <summary>The field's current label (read-only when the server cannot change it).</summary>
    public string Label
    {
        get => _label;
        set
        {
            if (SetField(ref _label, value))
            {
                OnPropertyChanged(nameof(LabelHint));
            }
        }
    }

    /// <summary>
    /// Whether the label can be changed after the field exists: the server's draft model has no
    /// such op today, so the editor is disabled and <see cref="LabelHint"/> says why.
    /// </summary>
    public bool CanEditLabel => _session.Draft?.Supports("set_label") ?? false;

    /// <summary>Why the label editor is disabled, or what editing it does.</summary>
    public string LabelHint => CanEditLabel
        ? "Changing the label renames it on the form; the storage name stays as it is."
        : "This server can set a label when the field is added, but has no change-a-label op yet.";

    /// <summary>The field's storage name (editable: renaming is a server op).</summary>
    public string Name
    {
        get => _name;
        set
        {
            if (SetField(ref _name, value))
            {
                NameError = DesignerNaming.IsStorageName(value) || string.IsNullOrEmpty(value)
                    ? string.Empty
                    : "Use lower-case letters, digits and underscores, starting with a letter.";
            }
        }
    }

    /// <summary>A client-side shape hint for the name (the server remains the authority).</summary>
    public string NameError
    {
        get => _nameError;
        private set => SetField(ref _nameError, value);
    }

    /// <summary>The field's type, in the palette's words.</summary>
    public string TypeTitle => Field is null ? string.Empty : FieldTypeCatalog.TitleOf(Field.Type);

    /// <summary>Whether the field is a choice list (so the options editor applies).</summary>
    public bool IsSelect => string.Equals(Field?.Type, "select", StringComparison.Ordinal);

    /// <summary>Whether the field is required.</summary>
    public bool Required
    {
        get => _required;
        set => SetField(ref _required, value);
    }

    /// <summary>The field's default, as text.</summary>
    public string DefaultValue
    {
        get => _defaultValue;
        set => SetField(ref _defaultValue, value);
    }

    /// <summary>The choice list being edited.</summary>
    public ObservableCollection<OptionRow> Options { get; } = new();

    /// <summary>The read-only flag the server reports (no op can change it yet).</summary>
    public bool ReadOnly { get; private set; }

    /// <summary>The hidden flag the server reports (no op can change it yet).</summary>
    public bool Hidden { get; private set; }

    /// <summary>The per-field validation message the server reports (no op can change it yet).</summary>
    public string ValidationText { get; private set; } = string.Empty;

    /// <summary>The read-only flag in a person's words.</summary>
    public string ReadOnlyText => ReadOnly ? "Read-only" : "People can edit it";

    /// <summary>The hidden flag in a person's words.</summary>
    public string HiddenText => Hidden ? "Hidden from the form" : "Shown on the form";

    /// <summary>The field's validation message, or what its absence means.</summary>
    public string ValidationTextOrNone => string.IsNullOrWhiteSpace(ValidationText) ? "No message of its own" : ValidationText;

    /// <summary>Closes the type-change dialog without applying anything.</summary>
    public void DismissTypeChange() => TypeChange = null;

    /// <summary>Why the read-only, hidden and validation-message rows are read-only here.</summary>
    public string UnsupportedPropertiesHint
    {
        get
        {
            var missing = new List<string>();
            if (!Supports("set_read_only"))
            {
                missing.Add("read-only");
            }

            if (!Supports("set_hidden"))
            {
                missing.Add("hidden");
            }

            if (!Supports("set_validation_text"))
            {
                missing.Add("a field's validation message");
            }

            return missing.Count == 0
                ? "Every property of the field has an editor."
                : $"This server's draft has no op for {Join(missing)} yet: the value is shown as the server reports it, and the editor stays disabled until the draft model can change it.";
        }
    }

    /// <summary>The visual condition builder's rows.</summary>
    public ConditionDraft Condition { get; }

    /// <summary>The builder's rows as the view binds them, kept in step with <see cref="Condition"/>.</summary>
    public ObservableCollection<ConditionRowDraft> ConditionRows { get; } = new();

    /// <summary>The operator choices the builder's rows can use.</summary>
    public IReadOnlyList<ConditionOperatorChoice> Operators { get; } = ConditionOperatorChoice.All;

    /// <summary>Whether the rows combine with "and" (false) or "or" (true).</summary>
    public bool UseOr
    {
        get => Condition.Join == ConditionJoin.Or;
        set
        {
            Condition.Join = value ? ConditionJoin.Or : ConditionJoin.And;
            OnPropertyChanged();
        }
    }

    /// <summary>The expression text the server compiled from the rows (the preview line).</summary>
    public string ConditionPreview
    {
        get => _conditionPreview;
        private set => SetField(ref _conditionPreview, value);
    }

    /// <summary>The server's complaint about the rows, in plain language, or empty.</summary>
    public string ConditionError
    {
        get => _conditionError;
        private set => SetField(ref _conditionError, value);
    }

    /// <summary>The field names the builder's rows can reference (all fields of the scope).</summary>
    public ObservableCollection<CanvasField> ConditionFields { get; } = new();

    /// <summary>The permissions templates with their rights.</summary>
    public IReadOnlyList<PermissionsTemplateInfo> Templates => _templates;

    /// <summary>The chosen permissions template.</summary>
    public string SelectedTemplate
    {
        get => _selectedTemplate;
        set
        {
            if (SetField(ref _selectedTemplate, value))
            {
                OnPropertyChanged(nameof(TemplateRights));
            }
        }
    }

    /// <summary>The chosen template as the picker's item (the view's two-way binding).</summary>
    public PermissionsTemplateInfo? SelectedTemplateChoice
    {
        get => PermissionsTemplates.Find(SelectedTemplate);
        set
        {
            if (value is not null && !string.Equals(value.Id, SelectedTemplate, StringComparison.Ordinal))
            {
                SelectedTemplate = value.Id;
                OnPropertyChanged(nameof(SelectedTemplateChoice));
            }
        }
    }

    /// <summary>Who the chosen template lets read, write and submit.</summary>
    public string TemplateRights
    {
        get
        {
            var template = PermissionsTemplates.Find(SelectedTemplate);
            return template is null
                ? string.Empty
                : $"Read: {template.Read}. Write: {template.Write}. Submit: {template.Submit}.";
        }
    }

    /// <summary>The naming series pattern (a DocType-level property).</summary>
    public string NamingSeries
    {
        get => _session.Draft?.NamingSeries ?? string.Empty;
        set
        {
            _namingDraft = value;
            OnPropertyChanged();
        }
    }

    private string? _namingDraft;

    /// <summary>The type change waiting for confirmation, if any.</summary>
    public TypeChangePlanSummary? TypeChange
    {
        get => _typeChange;
        private set
        {
            if (SetField(ref _typeChange, value))
            {
                OnPropertyChanged(nameof(HasTypeChange));
                OnPropertyChanged(nameof(TypeChangeText));
            }
        }
    }

    /// <summary>Whether a type change dialog is open.</summary>
    public bool HasTypeChange => TypeChange is not null;

    /// <summary>The classification, in a person's words, plus what happens to the values.</summary>
    public string TypeChangeText => TypeChange is null
        ? string.Empty
        : $"{TypeChange.ClassificationText}. {TypeChange.Summary}";

    /// <summary>Loads the inspector for a field (or clears it for null).</summary>
    public void Inspect(CanvasField? field, IReadOnlyList<CanvasField> scopeFields)
    {
        Field = field;
        Options.Clear();
        ConditionFields.Clear();
        Condition.Clear();
        ConditionPreview = string.Empty;
        ConditionError = string.Empty;
        TypeChange = null;
        if (field is null)
        {
            Label = string.Empty;
            Name = string.Empty;
            return;
        }

        foreach (var candidate in scopeFields)
        {
            ConditionFields.Add(candidate);
        }

        var snapshot = _session.Draft?.FieldsOf(field.ChildDoctype)
            .FirstOrDefault(f => string.Equals(f.Name, field.Name, StringComparison.Ordinal));
        Label = snapshot?.Label ?? field.Label;
        Name = field.Name;
        SelectedTypeChoice = FieldTypeCatalog.Find(field.Type);
        Required = snapshot?.Required ?? false;
        DefaultValue = snapshot?.Default ?? string.Empty;
        ReadOnly = snapshot?.ReadOnly ?? false;
        Hidden = snapshot?.Hidden ?? false;
        ValidationText = snapshot?.ValidationText ?? string.Empty;
        SelectedTemplate = _session.Draft?.PermissionsTemplate ?? "standard";
        foreach (var option in snapshot?.OptionList ?? Array.Empty<string>())
        {
            Options.Add(new OptionRow(option));
        }

        if (!string.IsNullOrEmpty(snapshot?.DependsOnText))
        {
            ConditionPreview = snapshot.DependsOnText;
        }

        SyncConditionRows();

        OnPropertyChanged(nameof(ReadOnly));
        OnPropertyChanged(nameof(Hidden));
        OnPropertyChanged(nameof(ValidationText));
        OnPropertyChanged(nameof(ReadOnlyText));
        OnPropertyChanged(nameof(HiddenText));
        OnPropertyChanged(nameof(ValidationTextOrNone));
        OnPropertyChanged(nameof(SelectedTemplateChoice));
        OnPropertyChanged(nameof(NamingSeries));
        OnPropertyChanged(nameof(SelectedTemplate));
        OnPropertyChanged(nameof(TemplateRights));
        OnPropertyChanged(nameof(UseOr));
    }

    /// <summary>Re-reads the values from the draft after a server change.</summary>
    public void RefreshFromDraft(IReadOnlyList<CanvasField> scopeFields)
    {
        var current = Field;
        Inspect(current, scopeFields);
    }

    /// <summary>Asks the server to compile the condition rows and shows the text or the refusal.</summary>
    public async Task RefreshConditionAsync()
    {
        var expression = Condition.ToExpression();
        if (expression is null)
        {
            ConditionPreview = string.Empty;
            ConditionError = string.Empty;
            return;
        }

        var (text, error) = await _session.PreviewConditionAsync(expression).ConfigureAwait(true);
        ConditionPreview = text ?? string.Empty;
        ConditionError = error ?? string.Empty;
    }

    /// <summary>Applies the condition to the selected field.</summary>
    public async Task ApplyConditionAsync()
    {
        if (Field is null)
        {
            return;
        }

        var expression = Condition.ToExpression();
        if (expression is not null && !string.IsNullOrEmpty(ConditionError))
        {
            return;
        }

        await _session.SetDependsOnAsync(Field.Name, expression, Field.ChildDoctype).ConfigureAwait(true);
        await RefreshConditionAsync().ConfigureAwait(true);
    }

    /// <summary>Clears the selected field's condition.</summary>
    public async Task ClearConditionAsync()
    {
        if (Field is null)
        {
            return;
        }

        Condition.Clear();
        SyncConditionRows();
        ConditionPreview = string.Empty;
        ConditionError = string.Empty;
        await _session.SetDependsOnAsync(Field.Name, null, Field.ChildDoctype).ConfigureAwait(true);
    }

    /// <summary>Asks the server how the type change would treat the stored values.</summary>
    public async Task PlanTypeChangeAsync(string newType)
    {
        if (Field is null)
        {
            return;
        }

        TypeChange = await _session.PlanTypeChangeAsync(Field.Name, newType, Field.ChildDoctype).ConfigureAwait(true);
    }

    /// <summary>Applies the classified type change when the server allows it.</summary>
    public async Task ApplyTypeChangeAsync()
    {
        if (TypeChange is null || !TypeChange.CanApply)
        {
            return;
        }

        var result = await _session.ConfirmTypeChangeAsync().ConfigureAwait(true);
        if (result.Ok)
        {
            TypeChange = null;
        }
    }

    private bool Supports(string op) => _session.Draft?.Supports(op) ?? false;

    private static string Join(List<string> parts) => parts.Count switch
    {
        0 => string.Empty,
        1 => parts[0],
        2 => $"{parts[0]} or {parts[1]}",
        _ => $"{string.Join(", ", parts.Take(parts.Count - 1))} or {parts[^1]}",
    };

    private async Task ApplyNameAsync()
    {
        if (Field is null || string.IsNullOrWhiteSpace(Name) || string.Equals(Name, Field.Name, StringComparison.Ordinal))
        {
            return;
        }

        await _session.RenameFieldAsync(Field.Name, Name.Trim(), Field.ChildDoctype).ConfigureAwait(true);
    }

    /// <summary>Sends <see cref="Required"/> to the server.</summary>
    public async Task ToggleRequiredAsync()
    {
        if (Field is null)
        {
            return;
        }

        await _session.SetRequiredAsync(Field.Name, Required, Field.ChildDoctype).ConfigureAwait(true);
    }

    private async Task ApplyDefaultAsync()
    {
        if (Field is null)
        {
            return;
        }

        var value = string.IsNullOrWhiteSpace(DefaultValue) ? null : DefaultValue.Trim();
        await _session.SetDefaultAsync(Field.Name, value, Field.ChildDoctype).ConfigureAwait(true);
    }

    private async Task ApplyOptionsAsync()
    {
        if (Field is null)
        {
            return;
        }

        var options = Options.Select(o => o.Text.Trim()).Where(t => t.Length > 0).ToList();
        await _session.SetOptionsAsync(Field.Name, options, Field.ChildDoctype).ConfigureAwait(true);
    }

    private async Task ApplyTemplateAsync()
    {
        await _session.SetPermissionsTemplateAsync(SelectedTemplate).ConfigureAwait(true);
    }

    private async Task ApplyNamingAsync()
    {
        var pattern = string.IsNullOrWhiteSpace(_namingDraft) ? null : _namingDraft.Trim();
        await _session.SetNamingSeriesAsync(pattern).ConfigureAwait(true);
    }

    private void AddConditionRow()
    {
        var first = ConditionFields.FirstOrDefault();
        Condition.Add(first?.Name ?? string.Empty, ConditionOperator.Equals);
        SyncConditionRows();
        OnPropertyChanged(nameof(UseOr));
    }

    private void RemoveConditionRow(object? parameter)
    {
        if (parameter is ConditionRowDraft row)
        {
            Condition.RemoveAt(Condition.Rows.ToList().IndexOf(row));
            SyncConditionRows();
        }
    }

    /// <summary>Rebuilds the view's rows from the builder's rows, giving each row its field choices.</summary>
    private void SyncConditionRows()
    {
        ConditionRows.Clear();
        var fields = ConditionFields.ToList();
        foreach (var row in Condition.Rows)
        {
            row.Fields = fields;
            ConditionRows.Add(row);
        }
    }

    private void RemoveOption(OptionRow? row)
    {
        if (row is not null)
        {
            Options.Remove(row);
        }
    }

    private void MoveOption(OptionRow? row, int delta)
    {
        if (row is null)
        {
            return;
        }

        var index = Options.IndexOf(row);
        var target = index + delta;
        if (index < 0 || target < 0 || target >= Options.Count)
        {
            return;
        }

        Options.Move(index, target);
    }
}
