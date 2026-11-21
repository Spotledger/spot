using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>One field on the canvas, in its section or in the "not on the form" tray.</summary>
public sealed record CanvasField(string Name, string Label, string Type, string? ChildDoctype = null)
{
    /// <summary>The palette's title for the field's type.</summary>
    public string TypeTitle => FieldTypeCatalog.TitleOf(Type);

    /// <summary>"Label (type)" for a list row.</summary>
    public string Summary => $"{Label} · {TypeTitle}";

    /// <summary>The label, for pickers that show the item itself.</summary>
    public override string ToString() => Label;
}

/// <summary>One section of the canvas: its heading, its columns and the fields it holds.</summary>
public sealed class CanvasSectionViewModel : ObservableObject
{
    private int _columns;

    /// <summary>Creates a section row.</summary>
    public CanvasSectionViewModel(string id, string title, int columns, string? tabId, string? childDoctype)
    {
        Id = id;
        Title = title;
        _columns = columns;
        TabId = tabId;
        ChildDoctype = childDoctype;
    }

    /// <summary>The section's id.</summary>
    public string Id { get; }

    /// <summary>The section's title.</summary>
    public string Title { get; }

    /// <summary>The tab it sits in, or null for the top level.</summary>
    public string? TabId { get; }

    /// <summary>The child DocType whose grid this is, or null for the root form.</summary>
    public string? ChildDoctype { get; }

    /// <summary>The fields in the section, in order.</summary>
    public ObservableCollection<CanvasField> Fields { get; } = new();

    /// <summary>The column count (the layout span the designer resizes).</summary>
    public int Columns
    {
        get => _columns;
        set
        {
            if (SetField(ref _columns, value))
            {
                OnPropertyChanged(nameof(SpanText));
            }
        }
    }

    /// <summary>"2 columns" for the section header.</summary>
    public string SpanText => Columns == 1 ? "1 column" : $"{Columns} columns";
}

/// <summary>A child DocType of the draft, shown as an embedded grid designer.</summary>
public sealed class ChildTableViewModel : ObservableObject
{
    /// <summary>Creates the child row.</summary>
    public ChildTableViewModel(string doctype, string label)
    {
        Doctype = doctype;
        Label = label;
    }

    /// <summary>The child DocType's name.</summary>
    public string Doctype { get; }

    /// <summary>Its label.</summary>
    public string Label { get; }

    /// <summary>The child's grid sections.</summary>
    public ObservableCollection<CanvasSectionViewModel> Sections { get; } = new();

    /// <summary>The child's fields that no section places.</summary>
    public ObservableCollection<CanvasField> Unplaced { get; } = new();
}

/// <summary>A palette group ("Text", "Numbers", ...) with its entries.</summary>
public sealed class PaletteGroupViewModel
{
    /// <summary>Creates the group.</summary>
    public PaletteGroupViewModel(string title, IReadOnlyList<FieldTypeInfo> fields)
    {
        Title = title;
        Fields = fields;
    }

    /// <summary>The group's heading.</summary>
    public string Title { get; }

    /// <summary>The group's field types.</summary>
    public IReadOnlyList<FieldTypeInfo> Fields { get; }
}

/// <summary>
/// The canvas: sections, columns and tabs for the form being designed, the field-type palette
/// that adds to it, the drag-to-reorder and drag-to-resize actions, and a keyboard route for every
/// one of those actions.
/// </summary>
/// <remarks>
/// Every mutating action ends in a <see cref="DesignerSession"/> command, so the same server ops
/// run whether the user dragged or pressed a key. The canvas keeps no layout rules of its own: it
/// reads the sections back from the server's draft after each accepted op.
/// </remarks>
public sealed class CanvasViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private string? _selectedSectionId;
    private CanvasField? _selectedField;
    private ChildTableViewModel? _activeChild;

    /// <summary>Creates the canvas over a session.</summary>
    public CanvasViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        Palette = FieldTypeCatalog.Groups()
            .Select(g => new PaletteGroupViewModel(g.Group, g.Fields))
            .ToList();
        Tabs = new ObservableCollection<string>();
        Rebuild();
    }

    /// <summary>Raised when the selected field, section or child grid changes.</summary>
    public event EventHandler? SelectionChanged;

    /// <summary>The palette groups, in the order the palette shows them.</summary>
    public IReadOnlyList<PaletteGroupViewModel> Palette { get; }

    /// <summary>The root form's sections.</summary>
    public ObservableCollection<CanvasSectionViewModel> Sections { get; } = new();

    /// <summary>The root fields that no section places yet.</summary>
    public ObservableCollection<CanvasField> Unplaced { get; } = new();

    /// <summary>The form's tabs.</summary>
    public ObservableCollection<string> Tabs { get; }

    /// <summary>The draft's child DocTypes, each an embedded grid designer.</summary>
    public ObservableCollection<ChildTableViewModel> ChildTables { get; } = new();

    /// <summary>The child grid being designed, or null when the root form is shown.</summary>
    public ChildTableViewModel? ActiveChild
    {
        get => _activeChild;
        private set
        {
            if (SetField(ref _activeChild, value))
            {
                OnPropertyChanged(nameof(IsChildGrid));
                OnPropertyChanged(nameof(ActiveScopeTitle));
            }
        }
    }

    /// <summary>Whether the canvas is showing a child's grid.</summary>
    public bool IsChildGrid => ActiveChild is not null;

    /// <summary>The heading of the canvas scope.</summary>
    public string ActiveScopeTitle => ActiveChild is null ? "Form" : $"{ActiveChild.Label} grid";

    /// <summary>The sections of the current scope (root form or child grid).</summary>
    public ObservableCollection<CanvasSectionViewModel> CurrentSections => ActiveChild?.Sections ?? Sections;

    /// <summary>The unplaced fields of the current scope.</summary>
    public ObservableCollection<CanvasField> CurrentUnplaced => ActiveChild?.Unplaced ?? Unplaced;

    /// <summary>The section the keyboard actions act on.</summary>
    public string? SelectedSectionId
    {
        get => _selectedSectionId;
        set
        {
            if (SetField(ref _selectedSectionId, value))
            {
                SelectionChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    /// <summary>The field the inspector shows.</summary>
    public CanvasField? SelectedField
    {
        get => _selectedField;
        set
        {
            if (SetField(ref _selectedField, value))
            {
                if (value is not null)
                {
                    OnPropertyChanged(nameof(SelectedFieldName));
                    OnPropertyChanged(nameof(SelectedFieldLabel));
                }

                SelectionChanged?.Invoke(this, EventArgs.Empty);
            }
        }
    }

    /// <summary>The selected field's storage name, or null.</summary>
    public string? SelectedFieldName => SelectedField?.Name;

    /// <summary>The selected field's label, or null.</summary>
    public string? SelectedFieldLabel => SelectedField?.Label;

    /// <summary>The child DocType of the current scope, or null for the root.</summary>
    public string? ScopeChildDoctype => ActiveChild?.Doctype;

    /// <summary>Reloads the canvas from the session's draft.</summary>
    public void Rebuild()
    {
        var draft = _session.Draft;
        Sections.Clear();
        Unplaced.Clear();
        Tabs.Clear();
        ChildTables.Clear();
        if (draft is null)
        {
            return;
        }

        Fill(draft, null, Sections, Unplaced);
        foreach (var tab in draft.Form.TabList)
        {
            Tabs.Add(tab.Title);
        }

        foreach (var child in draft.ChildList)
        {
            var vm = new ChildTableViewModel(child.Doctype, child.Label);
            Fill(draft, child.Doctype, vm.Sections, vm.Unplaced);
            ChildTables.Add(vm);
        }

        if (ActiveChild is not null)
        {
            ActiveChild = ChildTables.FirstOrDefault(c => string.Equals(c.Doctype, ActiveChild.Doctype, StringComparison.Ordinal));
        }

        if (SelectedField is { } selected)
        {
            SelectedField = CurrentSections.SelectMany(s => s.Fields)
                .Concat(CurrentUnplaced)
                .FirstOrDefault(f => string.Equals(f.Name, selected.Name, StringComparison.Ordinal) && string.Equals(f.ChildDoctype, selected.ChildDoctype, StringComparison.Ordinal))
                ?? selected;
        }

        OnPropertyChanged(nameof(CurrentSections));
        OnPropertyChanged(nameof(CurrentUnplaced));
        OnPropertyChanged(nameof(ActiveScopeTitle));
        OnPropertyChanged(nameof(IsChildGrid));
        SelectedSectionId ??= CurrentSections.FirstOrDefault()?.Id;
    }

    /// <summary>Switches the canvas to a child DocType's own grid layout.</summary>
    public void OpenChildGrid(string doctype)
    {
        ActiveChild = ChildTables.FirstOrDefault(c => string.Equals(c.Doctype, doctype, StringComparison.Ordinal));
        SelectedField = null;
        SelectedSectionId = ActiveChild?.Sections.FirstOrDefault()?.Id;
        OnPropertyChanged(nameof(CurrentSections));
        OnPropertyChanged(nameof(CurrentUnplaced));
    }

    /// <summary>Returns the canvas to the root form.</summary>
    public void CloseChildGrid()
    {
        ActiveChild = null;
        SelectedField = null;
        SelectedSectionId = Sections.FirstOrDefault()?.Id;
        OnPropertyChanged(nameof(CurrentSections));
        OnPropertyChanged(nameof(CurrentUnplaced));
    }

    /// <summary>
    /// Drops a palette type into a section, after a field (or at the end): the pointer's drop.
    /// An empty <paramref name="sectionId"/> drops onto the "not on the form" tray, which adds
    /// the field to the definition without placing it. A child-table entry adds the child DocType
    /// and its grid in the same run.
    /// </summary>
    public async Task<DesignerResult> DropFieldAsync(string type, string sectionId, string? afterField = null)
    {
        var placement = string.IsNullOrEmpty(sectionId) ? null : new DesignerPlacement(sectionId, afterField);
        var child = ScopeChildDoctype;
        var info = FieldTypeCatalog.Find(type) ?? throw new ArgumentException($"`{type}` is not in the palette.", nameof(type));
        DesignerResult result;
        if (info.IsChildTable)
        {
            var childDoctype = DesignerNaming.Unique(
                _session.Draft?.ChildList.Select(c => c.Doctype).ToList() ?? new List<string>(),
                DesignerNaming.SafeIdentifier($"{LabelOfCurrentDocType()} item"));
            result = await _session.AddChildTableAsync(info.Title, childDoctype, TitleCase(childDoctype), placement, cancellationToken: CancellationToken.None).ConfigureAwait(true);
        }
        else if (info.NeedsTarget)
        {
            // A link needs its target DocType from the picker; the designer view routes that
            // case to LinkPickerViewModel before calling here.
            return new DesignerResult(false, null, "Choose the DocType this field links to first.");
        }
        else
        {
            result = await _session.AddFieldAsync(info.Title, type, placement, child, cancellationToken: CancellationToken.None).ConfigureAwait(true);
        }

        if (result.Ok)
        {
            Rebuild();
            SelectFirstMatch(type);
        }

        return result;
    }

    /// <summary>Drops a palette type on the tray: the field joins the definition, not the form.</summary>
    public Task<DesignerResult> DropToTrayAsync(string type) => DropFieldAsync(type, string.Empty);

    /// <summary>
    /// Drops a field from one place to another: onto another section, or after another field.
    /// This is the same server op the keyboard's move gestures send.
    /// </summary>
    public async Task<DesignerResult> PlaceAsync(CanvasField field, string sectionId, string? afterField = null)
    {
        ArgumentNullException.ThrowIfNull(field);
        var result = await _session.MoveInLayoutAsync(field.Name, sectionId, afterField, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            Rebuild();
            SelectedSectionId = sectionId;
            SelectFirstMatch(field.Type, field.Name);
        }

        return result;
    }

    /// <summary>Takes a field off the form, back to the tray (a layout op, not a deletion).</summary>
    public async Task<DesignerResult> UnplaceAsync(CanvasField field)
    {
        ArgumentNullException.ThrowIfNull(field);
        var layout = _session.Draft?.LayoutOf(ScopeChildDoctype) ?? LayoutSnapshot.Empty;
        var section = layout.SectionList.FirstOrDefault(s => s.FieldItems.Contains(field.Name, StringComparer.Ordinal));
        var remaining = section?.FieldItems.Where(n => !string.Equals(n, field.Name, StringComparison.Ordinal)).ToList()
            ?? new List<string>();
        if (section is null)
        {
            return DesignerResult.Success;
        }

        var result = await _session.MoveInLayoutAsync(field.Name, section.Id, remaining.Count == 0 ? null : remaining[^1], ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            Rebuild();
        }

        return result;
    }

    /// <summary>Adds a section to the current form (or child grid), at the top level or in a tab.</summary>
    public async Task<DesignerResult> AddSectionAsync(string title, string? tabId = null, int? columns = null, string? id = null)
    {
        var resolved = string.IsNullOrWhiteSpace(title) ? "Section" : title.Trim();
        var taken = CurrentSections.Select(s => s.Id).ToList();
        var sectionId = id ?? DesignerNaming.Unique(taken, DesignerNaming.SafeIdentifier(resolved));
        var result = await _session.AddSectionAsync(sectionId, resolved, columns, tabId, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            Rebuild();
            SelectedSectionId = sectionId;
        }

        return result;
    }

    /// <summary>Adds a tab to the current form.</summary>
    public async Task<DesignerResult> AddTabAsync(string title)
    {
        var resolved = string.IsNullOrWhiteSpace(title) ? "Tab" : title.Trim();
        var taken = (_session.Draft?.LayoutOf(ScopeChildDoctype).TabList ?? Array.Empty<TabSnapshot>()).Select(t => t.Id).ToList();
        var id = DesignerNaming.Unique(taken, DesignerNaming.SafeIdentifier(resolved));
        var result = await _session.AddTabAsync(id, resolved, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            Rebuild();
        }

        return result;
    }

    /// <summary>Sets a section's column span exactly (the resize handle's drop).</summary>
    public async Task<DesignerResult> SetColumnsAsync(CanvasSectionViewModel section, int columns)
    {
        ArgumentNullException.ThrowIfNull(section);
        var clamped = Math.Clamp(columns, 1, 4);
        if (clamped == section.Columns)
        {
            return DesignerResult.Success;
        }

        SelectedSectionId = section.Id;
        var result = await _session.SetSectionColumnsAsync(section.Id, clamped, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            Rebuild();
        }

        return result;
    }

    /// <summary>Removes the selected field from the definition; the server asks for confirmation.</summary>
    public async Task<DesignerResult> RemoveSelectedAsync(bool confirmed = true)
    {
        if (SelectedField is not { } field)
        {
            return NoSelection();
        }

        var result = await _session.RemoveFieldAsync(field.Name, confirmed, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            SelectedField = null;
            Rebuild();
        }

        return result;
    }

    /// <summary>
    /// The keyboard route for a drop: insert a palette type at the selected position without a
    /// pointer. Identical server ops to <see cref="DropFieldAsync"/>.
    /// </summary>
    public Task<DesignerResult> InsertFieldAsync(string type, string sectionId, string? afterField = null) =>
        DropFieldAsync(type, sectionId, afterField);

    /// <summary>Moves the selected field one place up within its section (Alt+Up).</summary>
    public async Task<DesignerResult> MoveUpAsync()
    {
        var (field, section, index) = SelectedPosition();
        if (field is null || section is null || index < 0)
        {
            return NoSelection();
        }

        if (index == 0)
        {
            return new DesignerResult(false, null, "This is the first field; nothing is above it.");
        }

        if (index == 1)
        {
            // The layout's only reorder op anchors a field after another, and an absent anchor
            // means "the end", so the second field cannot reach the first position.
            return new DesignerResult(false, null, "The first field cannot be passed: reordering anchors a field after another field.");
        }

        return await _session.MoveInLayoutAsync(field.Name, section.Id, section.Fields[index - 2].Name, ScopeChildDoctype).ConfigureAwait(true);
    }

    /// <summary>Moves the selected field one place down within its section (Alt+Down).</summary>
    public async Task<DesignerResult> MoveDownAsync()
    {
        var (field, section, index) = SelectedPosition();
        if (field is null || section is null || index < 0)
        {
            return NoSelection();
        }

        if (index >= section.Fields.Count - 1)
        {
            return DesignerResult.Success;
        }

        // Dropping after the next field's successor puts the field one place lower.
        var after = index + 2 < section.Fields.Count ? section.Fields[index + 2].Name : null;
        return await _session.MoveInLayoutAsync(field.Name, section.Id, after, ScopeChildDoctype).ConfigureAwait(true);
    }

    /// <summary>Moves the selected field into the next section (Alt+Shift+Down).</summary>
    public async Task<DesignerResult> MoveToNextSectionAsync() => await MoveAcrossSectionAsync(1).ConfigureAwait(true);

    /// <summary>Moves the selected field into the previous section (Alt+Shift+Up).</summary>
    public async Task<DesignerResult> MoveToPrevSectionAsync() => await MoveAcrossSectionAsync(-1).ConfigureAwait(true);

    /// <summary>Narrows the selected section by one column (Alt+Left).</summary>
    public Task<DesignerResult> NarrowAsync() => ResizeAsync(-1);

    /// <summary>Widens the selected section by one column (Alt+Right).</summary>
    public Task<DesignerResult> WidenAsync() => ResizeAsync(1);

    /// <summary>Resizes the selected section's column span, clamped to the layout's 1..4.</summary>
    public Task<DesignerResult> ResizeAsync(int delta)
    {
        var section = SelectedSection();
        if (section is null)
        {
            return Task.FromResult(new DesignerResult(false, null, "Select a section first."));
        }

        var columns = Math.Clamp(section.Columns + delta, 1, 4);
        return columns == section.Columns
            ? Task.FromResult(DesignerResult.Success)
            : _session.SetSectionColumnsAsync(section.Id, columns, ScopeChildDoctype);
    }

    /// <summary>
    /// The one keyboard entry point: turns a gesture name into the canvas action a drag would
    /// perform. Returns false when the gesture is not a canvas gesture.
    /// </summary>
    public async Task<(bool Handled, DesignerResult Result)> HandleGestureAsync(string gesture, string? paletteType = null)
    {
        if (CanvasKeyboardMap.Resolve(gesture) is not { } kind)
        {
            return (false, DesignerResult.Success);
        }

        var result = kind switch
        {
            CanvasGesture.Insert => await InsertAtSelectionAsync(paletteType).ConfigureAwait(false),
            CanvasGesture.MoveUp => await MoveUpAsync().ConfigureAwait(false),
            CanvasGesture.MoveDown => await MoveDownAsync().ConfigureAwait(false),
            CanvasGesture.NextSection => await MoveToNextSectionAsync().ConfigureAwait(false),
            CanvasGesture.PreviousSection => await MoveToPrevSectionAsync().ConfigureAwait(false),
            CanvasGesture.Narrower => await NarrowAsync().ConfigureAwait(false),
            CanvasGesture.Wider => await WidenAsync().ConfigureAwait(false),
            _ => DesignerResult.Success,
        };
        Rebuild();
        return (true, result);
    }

    /// <summary>
    /// The keyboard route for a drop onto the selection: insert the palette type into the selected
    /// field's section, after that field (or at the end of the selected section).
    /// </summary>
    private Task<DesignerResult> InsertAtSelectionAsync(string? paletteType)
    {
        if (paletteType is null)
        {
            return Task.FromResult(new DesignerResult(false, null, "Pick a field type in the palette first."));
        }

        var section = SelectedPosition().Section;
        var sectionId = section?.Id ?? SelectedSectionId;
        if (sectionId is null)
        {
            return Task.FromResult(new DesignerResult(false, null, "Add a section, or select one, before inserting a field."));
        }

        var after = section is not null && SelectedField is not null ? SelectedField.Name : null;
        return InsertFieldAsync(paletteType, sectionId, after);
    }

    private async Task<DesignerResult> MoveAcrossSectionAsync(int direction)
    {
        var (field, section, _) = SelectedPosition();
        var sections = CurrentSections.ToList();
        if (field is null || section is null)
        {
            return NoSelection();
        }

        var target = sections.IndexOf(section) + direction;
        if (target < 0 || target >= sections.Count)
        {
            return DesignerResult.Success;
        }

        var destination = sections[target];
        var result = await _session.MoveInLayoutAsync(field.Name, destination.Id, destination.Fields.LastOrDefault()?.Name, ScopeChildDoctype).ConfigureAwait(true);
        if (result.Ok)
        {
            SelectedSectionId = destination.Id;
        }

        return result;
    }

    private (CanvasField? Field, CanvasSectionViewModel? Section, int Index) SelectedPosition()
    {
        if (SelectedField is not { } selected)
        {
            return (null, null, -1);
        }

        foreach (var section in CurrentSections)
        {
            for (var i = 0; i < section.Fields.Count; i++)
            {
                if (string.Equals(section.Fields[i].Name, selected.Name, StringComparison.Ordinal))
                {
                    return (section.Fields[i], section, i);
                }
            }
        }

        return (null, null, -1);
    }

    private CanvasSectionViewModel? SelectedSection()
    {
        if (SelectedSectionId is null)
        {
            return CurrentSections.FirstOrDefault();
        }

        return CurrentSections.FirstOrDefault(s => string.Equals(s.Id, SelectedSectionId, StringComparison.Ordinal))
            ?? CurrentSections.FirstOrDefault();
    }

    private static DesignerResult NoSelection() => new(false, null, "Select a field on the form first.");

    private void SelectFirstMatch(string type, string? name = null)
    {
        if (name is not null)
        {
            var exact = CurrentSections.SelectMany(s => s.Fields).Concat(CurrentUnplaced)
                .FirstOrDefault(f => string.Equals(f.Name, name, StringComparison.Ordinal));
            if (exact is not null)
            {
                SelectedField = exact;
                return;
            }
        }

        var match = CurrentSections.SelectMany(s => s.Fields).LastOrDefault(f => string.Equals(f.Type, type, StringComparison.Ordinal))
            ?? CurrentUnplaced.LastOrDefault(f => string.Equals(f.Type, type, StringComparison.Ordinal));
        if (match is not null)
        {
            SelectedField = match;
        }
    }

    private string LabelOfCurrentDocType() => _session.Draft?.Label ?? "Child";

    private static string TitleCase(string name)
    {
        var words = name.Split('_', StringSplitOptions.RemoveEmptyEntries);
        return string.Join(' ', words.Select(w => char.ToUpperInvariant(w[0]) + w[1..]));
    }

    private static void Fill(DraftDocument draft, string? childDoctype, ObservableCollection<CanvasSectionViewModel> sections, ObservableCollection<CanvasField> unplaced)
    {
        var fields = draft.FieldsOf(childDoctype).ToDictionary(f => f.Name, StringComparer.Ordinal);
        foreach (var section in draft.LayoutOf(childDoctype).SectionList)
        {
            var vm = new CanvasSectionViewModel(section.Id, section.Title, section.Columns ?? 1, section.TabId, childDoctype);
            foreach (var name in section.FieldItems)
            {
                if (fields.TryGetValue(name, out var field))
                {
                    vm.Fields.Add(new CanvasField(field.Name, field.Label, field.Type, childDoctype));
                }
            }

            sections.Add(vm);
        }

        foreach (var field in draft.UnplacedFields(childDoctype))
        {
            unplaced.Add(new CanvasField(field.Name, field.Label, field.Type, childDoctype));
        }
    }
}
