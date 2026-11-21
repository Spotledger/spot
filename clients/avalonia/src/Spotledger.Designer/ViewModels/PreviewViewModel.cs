using System.Collections.ObjectModel;

namespace Spotledger.Client.Designer;

/// <summary>One field of the previewed form, ready for the real control factory.</summary>
public sealed record PreviewFieldRow(string SectionId, PreviewFieldSnapshot Field)
{
    /// <summary>The palette's title for the field's type.</summary>
    public string TypeTitle => FieldTypeCatalog.TitleOf(Field.Type);

    /// <summary>The sample value the preview shows.</summary>
    public string Sample => Field.Sample ?? string.Empty;
}

/// <summary>
/// The preview pane: the form the server previews, with sample data, at desktop and narrow widths
/// and as the list view. The board it renders from is an in-memory document: the preview never
/// writes anything anywhere.
/// </summary>
public sealed class PreviewViewModel : ObservableObject
{
    private readonly DesignerSession _session;
    private PreviewLayout _layout = PreviewLayout.Desktop;
    private PreviewBoard? _board;
    private string _status = "The preview shows the form as it would render, with sample values.";

    /// <summary>Creates the pane over a session.</summary>
    public PreviewViewModel(DesignerSession session)
    {
        _session = session ?? throw new ArgumentNullException(nameof(session));
        ShowDesktopCommand = new AsyncRelayCommand(_ => ShowAsync(PreviewLayout.Desktop));
        ShowNarrowCommand = new AsyncRelayCommand(_ => ShowAsync(PreviewLayout.Narrow));
        ShowListCommand = new AsyncRelayCommand(_ => ShowAsync(PreviewLayout.List));
        RefreshCommand = new AsyncRelayCommand(_ => ShowAsync(CurrentLayout));
    }

    /// <summary>Shows the desktop width.</summary>
    public AsyncRelayCommand ShowDesktopCommand { get; }

    /// <summary>Shows the narrow width.</summary>
    public AsyncRelayCommand ShowNarrowCommand { get; }

    /// <summary>Shows the list view.</summary>
    public AsyncRelayCommand ShowListCommand { get; }

    /// <summary>Re-renders the current layout.</summary>
    public AsyncRelayCommand RefreshCommand { get; }

    /// <summary>Raised after the preview has been rebuilt.</summary>
    public event EventHandler? PreviewChanged;

    /// <summary>Which layout is showing.</summary>
    public PreviewLayout CurrentLayout => _layout;

    /// <summary>Whether the list view is showing.</summary>
    public bool IsList => _layout == PreviewLayout.List;

    /// <summary>Whether the narrow (single-column) form is showing.</summary>
    public bool IsNarrow => _layout == PreviewLayout.Narrow;

    /// <summary>Whether the desktop form is showing.</summary>
    public bool IsDesktop => _layout == PreviewLayout.Desktop;

    /// <summary>The preview's own summary line.</summary>
    public string Status
    {
        get => _status;
        private set => SetField(ref _status, value);
    }

    /// <summary>The last preview the server built.</summary>
    public PreviewModel? Preview { get; private set; }

    /// <summary>The preview's in-memory board (never persisted).</summary>
    public PreviewBoard? Board
    {
        get => _board;
        private set
        {
            if (SetField(ref _board, value))
            {
                OnPropertyChanged(nameof(Sections));
            }
        }
    }

    /// <summary>The previewed form's sections with the fields they show, in order.</summary>
    public ObservableCollection<PreviewFieldRow> Sections { get; } = new();

    /// <summary>The list view's columns.</summary>
    public ObservableCollection<string> ListColumns { get; } = new();

    /// <summary>The fields no section places, shown after the sections.</summary>
    public ObservableCollection<PreviewFieldSnapshot> LooseFields { get; } = new();

    /// <summary>
    /// Builds the preview from the server and fills the board with sample data. Nothing is saved:
    /// the preview is not the document.
    /// </summary>
    public async Task ShowAsync(PreviewLayout layout)
    {
        _layout = layout;
        Preview = await _session.PreviewAsync(layout).ConfigureAwait(true);
        Board = PreviewBoard.From(Preview);
        Sections.Clear();
        LooseFields.Clear();
        ListColumns.Clear();
        var placed = new List<string>();
        foreach (var section in Preview.FormLayout.SectionList)
        {
            foreach (var name in section.FieldItems)
            {
                if (Preview.Field(name) is { } field)
                {
                    Sections.Add(new PreviewFieldRow(section.Id, field));
                    placed.Add(name);
                }
            }
        }

        foreach (var field in Preview.FieldList.Where(f => !placed.Contains(f.Name, StringComparer.Ordinal)))
        {
            LooseFields.Add(field);
        }

        foreach (var field in Preview.FieldList)
        {
            ListColumns.Add(field.Label);
        }

        foreach (var field in Preview.FieldList)
        {
            if (!placed.Contains(field.Name, StringComparer.Ordinal))
            {
                Sections.Add(new PreviewFieldRow(string.Empty, field));
            }
        }

        Status = Preview.Valid
            ? $"{Preview.Label}: {Sections.Count} fields, {Preview.ChildList.Count} child table(s), sample values only."
            : $"{Preview.Label}: the preview shows {Preview.IssueList.Count} issue(s) the server reports.";
        OnPropertyChanged(nameof(IsList));
        OnPropertyChanged(nameof(IsNarrow));
        OnPropertyChanged(nameof(IsDesktop));
        OnPropertyChanged(nameof(Preview));
        PreviewChanged?.Invoke(this, EventArgs.Empty);
    }
}
