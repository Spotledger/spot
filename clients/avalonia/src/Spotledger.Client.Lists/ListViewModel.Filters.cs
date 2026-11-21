using Spotledger.Client.Metadata;

namespace Spotledger.Client.Lists;

public sealed partial class ListViewModel
{
    private void RaiseFilters()
    {
        FiltersChanged?.Invoke(this, EventArgs.Empty);
        RaiseState();
    }

    /// <summary>Builds the filter from the chips and the quick search.</summary>
    private bool TryBuildFilter(IReadOnlyList<FilterCondition> conditions, string search, out ListFilter? filter, out string? error)
    {
        filter = null;
        error = null;
        if (_model is null)
        {
            error = "The list is not ready.";
            return false;
        }

        // A condition on a field the person cannot see in the clear (hidden, masked, not offered) is refused
        // however it got here (a saved filter outlives a change of rights): a filter on it would reveal its values.
        var allowed = FilterFields;
        foreach (var condition in conditions)
        {
            if (!allowed.Any(f => f.Name == condition.Field))
            {
                error = "That field cannot be filtered.";
                return false;
            }
        }

        if (!FilterCondition.TryBuildAll(_model, conditions, out var built, out error))
        {
            return false;
        }

        var parts = new List<ListFilter>();
        if (built is AndFilter and && conditions.Count > 1)
        {
            parts.AddRange(and.Items);
        }
        else if (built is not null)
        {
            parts.Add(built);
        }

        var text = search.Trim();
        if (text.Length > 0 && _searchFields.Count > 0)
        {
            var any = _searchFields.Select(f => (ListFilter)new TextFilter(f.Name, TextOp.Contains, text, true)).ToArray();
            parts.Add(any.Length == 1 ? any[0] : new OrFilter(any));
        }

        filter = parts.Count switch { 0 => null, 1 => parts[0], _ => new AndFilter(parts) };
        if (!ListLimits.Fits(filter))
        {
            filter = null;
            error = "That is more filters than the server accepts. Remove one first.";
            return false;
        }

        return true;
    }

    private bool Reapply(IReadOnlyList<FilterCondition> conditions, string search)
    {
        string? error = null;
        ListFilter? filter = null;
        if (_source is null || !TryBuildFilter(conditions, search, out filter, out error))
        {
            Message = error;
            RaiseState();
            return false;
        }

        _conditions.Clear();
        _conditions.AddRange(conditions);
        _appliedSearch = search.Trim();
        Message = null;
        _source.Apply(filter, _source.Order);
        RaiseFilters();
        return true;
    }

    /// <summary>Adds a condition from the builder. Returns false and sets <see cref="Message"/> when it is invalid.</summary>
    public bool AddCondition(FilterCondition condition)
    {
        ArgumentNullException.ThrowIfNull(condition);
        if (_model?.Field(condition.Field) is not { } field || !FilterFields.Contains(field))
        {
            Message = "That field cannot be filtered.";
            RaiseState();
            return false;
        }

        return Reapply([.. _conditions, condition], _appliedSearch);
    }

    /// <summary>Removes the chip at <paramref name="index"/>.</summary>
    public void RemoveCondition(int index)
    {
        if (index < 0 || index >= _conditions.Count)
        {
            return;
        }

        var next = _conditions.ToList();
        next.RemoveAt(index);
        Reapply(next, _appliedSearch);
    }

    /// <summary>Clears every chip and the quick search.</summary>
    public void ClearFilters()
    {
        _searchText = string.Empty;
        Reapply([], string.Empty);
        OnPropertyChanged(nameof(SearchText));
    }

    /// <summary>The search box changed. The query runs after the debounce; a newer keystroke cancels the wait.</summary>
    public Task SetSearchTextAsync(string text)
    {
        _searchText = (text ?? string.Empty);
        if (_searchText.Length > 200)
        {
            _searchText = _searchText[..200];
        }

        _searchCts.Cancel();
        _searchCts.Dispose();
        _searchCts = new CancellationTokenSource();
        var token = _searchCts.Token;
        var pending = _searchText;
        return RunSearchAsync(pending, token);
    }

    private async Task RunSearchAsync(string pending, CancellationToken token)
    {
        try
        {
            var delay = _options.Delay ?? Task.Delay;
            await delay(_options.SearchDebounce, token).ConfigureAwait(true);
            token.ThrowIfCancellationRequested();
        }
        catch (OperationCanceledException)
        {
            return;
        }

        if (pending.Trim() != _appliedSearch)
        {
            Reapply(_conditions.ToList(), pending);
        }
    }

    /// <summary>Saves the current chips and search under <paramref name="name"/> (replacing one of that name).</summary>
    public async Task SaveCurrentFilterAsync(string name, CancellationToken cancellationToken = default)
    {
        if (string.IsNullOrWhiteSpace(name) || (_conditions.Count == 0 && _appliedSearch.Length == 0))
        {
            return;
        }

        var saved = _preferences.SavedFilters.Where(f => f.Name != name.Trim()).ToList();
        saved.Add(new SavedFilter(name.Trim(), _conditions.ToArray(), _appliedSearch.Length == 0 ? null : _appliedSearch));
        _preferences = ListPreferences.Normalize(_preferences with { SavedFilters = saved });
        await _store.SaveAsync(_user, DocType, _preferences, cancellationToken).ConfigureAwait(true);
        RaiseState();
    }

    /// <summary>Applies a saved filter by name. A saved condition that no longer fits the metadata refuses the whole filter.</summary>
    public bool ApplySavedFilter(string name)
    {
        var saved = _preferences.SavedFilters.FirstOrDefault(f => f.Name == name);
        if (saved is null)
        {
            return false;
        }

        _searchText = saved.Search ?? string.Empty;
        OnPropertyChanged(nameof(SearchText));
        return Reapply(saved.Conditions.ToList(), _searchText);
    }

    /// <summary>Deletes a saved filter.</summary>
    public async Task DeleteSavedFilterAsync(string name, CancellationToken cancellationToken = default)
    {
        _preferences = _preferences with { SavedFilters = _preferences.SavedFilters.Where(f => f.Name != name).ToArray() };
        await _store.SaveAsync(_user, DocType, _preferences, cancellationToken).ConfigureAwait(true);
        RaiseState();
    }

    // ------------------------------------------------------------------ columns

    /// <summary>Sets the visible columns in order (the chooser). Unknown or unreadable fields are ignored; at least one stays.</summary>
    public async Task SetColumnsAsync(IReadOnlyList<ColumnPreference> columns, CancellationToken cancellationToken = default)
    {
        if (_source is null || _permissions is null)
        {
            return;
        }

        var layout = ListColumnPlanner.Layout(_readable, _defaults, columns);
        await ApplyLayoutAsync(layout, cancellationToken).ConfigureAwait(true);
    }

    private async Task ApplyLayoutAsync(IReadOnlyList<(FieldModel Field, double? Width)> layout, CancellationToken cancellationToken)
    {
        if (_source is null)
        {
            return;
        }

        _columnSet = new ListColumnSet(ListColumnPlanner.Build(layout, _permissions), _formatter);
        _searchFields = ListColumnPlanner.SearchFields(_readable, layout.Select(l => l.Field).ToArray(), _permissions);
        var order = _source.Order.Where(o => layout.Any(l => l.Field.Name == o.Field)).ToArray();
        _preferences = _preferences with { Columns = layout.Select(l => new ColumnPreference(l.Field.Name, l.Width)).ToArray() };
        OnPropertyChanged(nameof(Columns));
        ColumnsChanged?.Invoke(this, EventArgs.Empty);
        TryBuildFilter(_conditions, _appliedSearch, out var filter, out _);
        _source.Apply(filter, order, _columnSet);
        await _store.SaveAsync(_user, DocType, _preferences, cancellationToken).ConfigureAwait(true);
        RaiseState();
    }

    /// <summary>Restores the view's default columns.</summary>
    public Task ResetColumnsAsync(CancellationToken cancellationToken = default)
        => ApplyLayoutAsync(_defaults.Select(f => (f, (double?)null)).ToArray(), cancellationToken);

    /// <summary>The grid moved a column: keep the cell order in step with the grid's model.</summary>
    public Task MoveColumnAsync(int from, int to, CancellationToken cancellationToken = default)
    {
        if (_columnSet is null || from == to || from < 0 || to < 0 || from >= _columnSet.Columns.Count || to >= _columnSet.Columns.Count)
        {
            return Task.CompletedTask;
        }

        var list = _columnSet.Columns.Select(c => (c.Field, _preferences.Columns?.FirstOrDefault(p => p.Field == c.Field.Name)?.Width)).ToList();
        var item = list[from];
        list.RemoveAt(from);
        list.Insert(to, item);
        return ApplyLayoutAsync(list, cancellationToken);
    }

    /// <summary>The grid resized a column: remember the width.</summary>
    public async Task SetColumnWidthAsync(int columnIndex, double width, CancellationToken cancellationToken = default)
    {
        if (_columnSet is null || columnIndex < 0 || columnIndex >= _columnSet.Columns.Count || !double.IsFinite(width))
        {
            return;
        }

        var current = _preferences.Columns ?? _columnSet.Columns.Select(c => new ColumnPreference(c.Field.Name)).ToArray();
        var name = _columnSet.Columns[columnIndex].Field.Name;
        var next = _columnSet.Columns.Select(c =>
        {
            var old = current.FirstOrDefault(p => p.Field == c.Field.Name);
            return new ColumnPreference(c.Field.Name, c.Field.Name == name ? width : old?.Width);
        }).ToArray();
        _preferences = ListPreferences.Normalize(_preferences with { Columns = next });
        await _store.SaveAsync(_user, DocType, _preferences, cancellationToken).ConfigureAwait(true);
    }

    /// <summary>The stored layout (visible fields in order, with widths), for tests and the chooser.</summary>
    public IReadOnlyList<ColumnPreference> CurrentLayout
        => _columnSet?.Columns.Select(c => new ColumnPreference(c.Field.Name, _preferences.Columns?.FirstOrDefault(p => p.Field == c.Field.Name)?.Width)).ToArray() ?? [];
}

public sealed partial class ListViewModel
{
    /// <summary>Asks for a name (through the shell) and saves the current filter under it.</summary>
    public async Task<bool> PromptSaveFilterAsync(CancellationToken cancellationToken = default)
    {
        if (_conditions.Count == 0 && _appliedSearch.Length == 0)
        {
            return false;
        }

        var name = await _shell.AskTextAsync("Save filter", "Name for this filter", true).ConfigureAwait(true);
        if (string.IsNullOrWhiteSpace(name))
        {
            return false;
        }

        await SaveCurrentFilterAsync(name, cancellationToken).ConfigureAwait(true);
        return true;
    }
}
