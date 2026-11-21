namespace Spotledger.Client.Designer;

/// <summary>One row of the shell's tool list: what the tool is called and the command that opens it.</summary>
public sealed class DesignerToolItem : ObservableObject
{
    private bool _isActive;

    /// <summary>Creates the row.</summary>
    /// <param name="tool">The registered tool.</param>
    /// <param name="open">How the shell opens a tool (the list owns that, not the row).</param>
    public DesignerToolItem(IDesignerTool tool, Action<DesignerToolItem> open)
    {
        Tool = tool ?? throw new ArgumentNullException(nameof(tool));
        ArgumentNullException.ThrowIfNull(open);
        OpenCommand = new RelayCommand(_ => open(this));
    }

    /// <summary>The registered tool.</summary>
    public IDesignerTool Tool { get; }

    /// <summary>The tool's stable id.</summary>
    public string Id => Tool.Id;

    /// <summary>The name shown in the list.</summary>
    public string Title => Tool.Title;

    /// <summary>The one-line description, shown as the button's tooltip.</summary>
    public string Description => Tool.Description;

    /// <summary>Opens this tool.</summary>
    public RelayCommand OpenCommand { get; }

    /// <summary>Whether this tool is the one currently shown.</summary>
    public bool IsActive
    {
        get => _isActive;
        internal set => SetField(ref _isActive, value);
    }
}

/// <summary>
/// The shell's tool list: the tools the composition root registered, the one that is open, and a
/// plain sentence when a tool could not be created.
/// </summary>
/// <remarks>
/// The list never creates a tool's server surface and never runs a tool's work. It calls
/// <see cref="IDesignerTool.Create"/> inside a guard, so a tool that fails to build (a missing
/// server surface, a bug in the tool) leaves the DocType designer usable and the person with a
/// sentence to read instead of a crashed window.
/// </remarks>
public sealed class DesignerToolListViewModel : ObservableObject
{
    private readonly DesignerToolContext _context;
    private object? _activeViewModel;
    private DesignerToolItem? _activeTool;
    private string _message = string.Empty;

    /// <summary>Creates the list over a registry and the connection the shell holds.</summary>
    /// <param name="registry">The tools the composition root registered.</param>
    /// <param name="context">The connection and the item a tool edits.</param>
    public DesignerToolListViewModel(DesignerToolRegistry registry, DesignerToolContext context)
    {
        ArgumentNullException.ThrowIfNull(registry);
        _context = context ?? throw new ArgumentNullException(nameof(context));
        Tools = registry.Tools.Select(tool => new DesignerToolItem(tool, item => Open(item.Id))).ToList();
        CloseCommand = new RelayCommand(_ => Close(), _ => HasActiveTool);
    }

    /// <summary>Every registered tool, in the order the composition root registered them.</summary>
    public IReadOnlyList<DesignerToolItem> Tools { get; }

    /// <summary>Whether there is a tool to list (a designer with no builders shows no strip).</summary>
    public bool HasTools => Tools.Count > 0;

    /// <summary>The view-model of the open tool; the view shows it through the app's data templates.</summary>
    public object? ActiveViewModel
    {
        get => _activeViewModel;
        private set
        {
            if (SetField(ref _activeViewModel, value))
            {
                OnPropertyChanged(nameof(HasActiveTool));
                OnPropertyChanged(nameof(ActiveTitle));
                CloseCommand.RaiseCanExecuteChanged();
            }
        }
    }

    /// <summary>Whether a tool is open.</summary>
    public bool HasActiveTool => ActiveViewModel is not null;

    /// <summary>The open tool, or null.</summary>
    public DesignerToolItem? ActiveTool
    {
        get => _activeTool;
        private set
        {
            if (SetField(ref _activeTool, value))
            {
                OnPropertyChanged(nameof(ActiveTitle));
            }
        }
    }

    /// <summary>The open tool's title, for the panel's heading.</summary>
    public string ActiveTitle => ActiveTool?.Title ?? string.Empty;

    /// <summary>A plain sentence about a tool that could not be opened; empty when all is well.</summary>
    public string Message
    {
        get => _message;
        private set
        {
            if (SetField(ref _message, value))
            {
                OnPropertyChanged(nameof(HasMessage));
            }
        }
    }

    /// <summary>Whether there is something to say in the tool strip.</summary>
    public bool HasMessage => Message.Length > 0;

    /// <summary>Closes the open tool and goes back to the DocType designer.</summary>
    public RelayCommand CloseCommand { get; }

    /// <summary>
    /// Opens the tool with this id and returns its view-model, or null when it could not be
    /// created (the reason is in <see cref="Message"/>). An unknown id says so the same way.
    /// </summary>
    /// <param name="id">The tool's id.</param>
    public object? Open(string id)
    {
        var item = Tools.FirstOrDefault(t => string.Equals(t.Id, id, StringComparison.Ordinal));
        if (item is null)
        {
            Show(null, null, $"There is no tool called `{id}`.");
            return null;
        }

        object? viewModel;
        try
        {
            viewModel = item.Tool.Create(_context)
                ?? throw new InvalidOperationException("it returned no view-model");
        }
        catch (Exception exception)
        {
            Show(null, null, $"The {item.Title.ToLowerInvariant()} could not be opened: {exception.Message}");
            return null;
        }

        Message = string.Empty;
        Show(item, viewModel, string.Empty);
        return viewModel;
    }

    /// <summary>Closes the open tool.</summary>
    public void Close() => Show(null, null, string.Empty);

    private void Show(DesignerToolItem? tool, object? viewModel, string message)
    {
        foreach (var row in Tools)
        {
            row.IsActive = ReferenceEquals(row, tool);
        }

        ActiveTool = tool;
        ActiveViewModel = viewModel;
        Message = message;
    }
}
