using System.ComponentModel;
using System.Runtime.CompilerServices;
using System.Windows.Input;
using Spotledger.Demo.Mock;

namespace Spotledger.Demo;

public abstract class ObservableModel : INotifyPropertyChanged
{
    public event PropertyChangedEventHandler? PropertyChanged;
    protected void Changed([CallerMemberName] string? name = null) => PropertyChanged?.Invoke(this, new(name));
}
public sealed class RelayCommand(Func<Task> execute) : ICommand
{
    public event EventHandler? CanExecuteChanged { add { } remove { } }
    public bool CanExecute(object? parameter) => true;
    public async void Execute(object? parameter) => await execute();
}
public enum DirtyChoice { Save, Discard, Cancel }
public enum SidebarMode { Expanded, Icons, Hidden }
public sealed class DemoTab(string key, string title, Guid companyId, DemoRecord? record = null) : ObservableModel
{
    public string Key { get; } = key;
    public string Title { get; } = title;
    public Guid CompanyId { get; } = companyId;
    public DemoRecord? Record { get; set; } = record;
    public string Note { get; private set; } = record?.Note ?? "";
    public bool IsDirty { get; private set; }
    public bool IsPinned { get; set; }
    // Presentation state survives tab switches; no backend rules here.
    public string Filter { get; set; } = "";
    public double ScrollOffset { get; set; }
    public string? FocusName { get; set; }
    public void Edit(string note) { Note = note; IsDirty = note != Record?.Note; Changed(nameof(IsDirty)); }
    public void Accept(DemoRecord record) { Record = record; Note = record.Note; IsDirty = false; Changed(nameof(IsDirty)); }
}
public sealed class TabManager
{
    private readonly List<DemoTab> tabs = [];
    public IReadOnlyList<DemoTab> Tabs => tabs;
    public DemoTab? Active { get; private set; }
    public DemoTab Open(string key, string title, Guid company, DemoRecord? record = null)
    {
        var existing = tabs.FirstOrDefault(t => t.Key == key);
        if (existing is null) { existing = new(key, title, company, record); tabs.Add(existing); }
        Active = existing;
        return existing;
    }
    public void Activate(DemoTab tab) { if (tabs.Contains(tab)) Active = tab; }
    public void Cycle(int direction)
    {
        if (tabs.Count > 0) Active = tabs[(tabs.IndexOf(Active!) + direction + tabs.Count) % tabs.Count];
    }
    public void Remove(IEnumerable<DemoTab> targets)
    {
        foreach (var target in targets.ToArray()) tabs.Remove(target);
        if (Active is null || !tabs.Contains(Active)) Active = tabs.LastOrDefault();
    }
    public IReadOnlyList<DemoTab> Others(DemoTab tab) => tabs.Where(t => t != tab && !t.IsPinned).ToArray();
    public IReadOnlyList<DemoTab> RightOf(DemoTab tab) => tabs.Skip(tabs.IndexOf(tab) + 1).Where(t => !t.IsPinned).ToArray();
}
