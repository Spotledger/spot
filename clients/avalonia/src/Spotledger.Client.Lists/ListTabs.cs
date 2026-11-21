using Avalonia;
using Avalonia.Controls;
using Spotledger.Client.Shell;

namespace Spotledger.Client.Lists;

/// <summary>The list as a workspace tab kind: one tab per DocType, registered in the shell's registry.</summary>
public static class ListTabs
{
    /// <summary>The tab kind name.</summary>
    public const string Kind = "list";

    /// <summary>The unique key of a DocType's list tab.</summary>
    public static string KeyFor(string doctype) => "list:" + doctype;

    /// <summary>The request that opens the list of <paramref name="doctype"/>.</summary>
    public static WorkspaceTabRequest Open(string doctype, string? title = null)
    {
        ArgumentException.ThrowIfNullOrWhiteSpace(doctype);
        return new WorkspaceTabRequest(Kind, KeyFor(doctype), title ?? doctype,
            new Dictionary<string, string>(StringComparer.Ordinal) { ["doctype"] = doctype });
    }

    /// <summary>The DocType a list request names.</summary>
    public static string ParseRequest(WorkspaceTabRequest request)
    {
        ArgumentNullException.ThrowIfNull(request);
        return request.Arguments.TryGetValue("doctype", out var doctype) && ListLimits.IsIdentifier(doctype)
            ? doctype
            : throw new ArgumentException("A list request names its DocType.", nameof(request));
    }

    /// <summary>Registers the list kind: <paramref name="create"/> builds the view model for a DocType.</summary>
    public static void Register(WorkspaceTabKinds kinds, Func<string, ListViewModel> create)
    {
        ArgumentNullException.ThrowIfNull(kinds);
        ArgumentNullException.ThrowIfNull(create);
        kinds.Register(Kind, request => create(ParseRequest(request)));
    }

    /// <summary>Keeps a tab title in step with its list.</summary>
    public static IDisposable Bind(WorkspaceTabViewModel tab, ListViewModel list)
    {
        ArgumentNullException.ThrowIfNull(tab);
        ArgumentNullException.ThrowIfNull(list);
        void Sync() => tab.Title = list.Title;
        void Changed(object? sender, System.ComponentModel.PropertyChangedEventArgs e)
        {
            if (e.PropertyName == nameof(ListViewModel.Title) || e.PropertyName == nameof(ListViewModel.Phase))
            {
                Sync();
            }
        }

        list.PropertyChanged += Changed;
        Sync();
        return new Unsubscribe(() => list.PropertyChanged -= Changed);
    }

    private sealed class Unsubscribe(Action action) : IDisposable
    {
        public void Dispose() => action();
    }
}

/// <summary>Flags of the list project.</summary>
public static class ListFlags
{
    /// <summary>The minimal list-and-form demo of the split arrangement (the full workspace is WP-UI-17).
    /// Off unless the host turns it on.</summary>
    public static bool SplitDemo { get; set; }
}

/// <summary>A minimal two-pane arrangement: the list on one side, a detail control on the other, driven by
/// <see cref="ListViewModel.SelectedRowChanged"/>. Behind <see cref="ListFlags.SplitDemo"/>; it is a demo of
/// the event, not the workspace.</summary>
public sealed class ListSplitView : Avalonia.Controls.Grid
{
    private readonly ContentControl _detail = new();

    /// <summary>Creates the arrangement. <paramref name="detail"/> builds the right pane for a selected document id.</summary>
    public ListSplitView(ListView list, Func<string, string, Control> detail)
    {
        ArgumentNullException.ThrowIfNull(list);
        ArgumentNullException.ThrowIfNull(detail);
        if (!ListFlags.SplitDemo)
        {
            Children.Add(list);
            return;
        }

        ColumnDefinitions.Add(new ColumnDefinition(2, GridUnitType.Star));
        ColumnDefinitions.Add(new ColumnDefinition(3, GridUnitType.Star));
        SetColumn(_detail, 1);
        Children.Add(list);
        Children.Add(_detail);
        list.Model.SelectedRowChanged += (_, e) => _detail.Content = e.Id is null ? null : detail(e.DocType, e.Id);
    }

    /// <summary>The right pane's content (tests).</summary>
    public object? Detail => _detail.Content;
}
