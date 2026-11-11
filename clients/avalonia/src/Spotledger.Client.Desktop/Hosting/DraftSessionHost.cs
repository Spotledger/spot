using Spotledger.Client.Workspace;

namespace Spotledger.Client.Desktop.Hosting;

/// <summary>
/// Owns the draft store and the autosave coordinator of the one signed-in user on one server, and
/// nothing else. A switch of user or server flushes the old coordinator to <b>its own</b> scope and
/// disposes it (and its store) before the new one exists, so no code path reads or writes a draft with
/// another user's or server's key. Sign-out removes every draft of the scope and the key that protected
/// them; a wipe that left something behind is shown and retried at the next start.
/// </summary>
internal sealed class DraftSessionHost : IDisposable
{
    private readonly Func<DraftScope, IDraftStore> _storeFactory;
    private readonly string? _pendingFolder;
    private readonly TimeProvider _clock;
    private readonly AutosaveOptions _options;
    private readonly SemaphoreSlim _gate = new(1, 1);

    public DraftSessionHost(
        Func<DraftScope, IDraftStore> storeFactory,
        string? pendingWipeFolder = null,
        TimeProvider? clock = null,
        AutosaveOptions? options = null)
    {
        _storeFactory = storeFactory ?? throw new ArgumentNullException(nameof(storeFactory));
        _pendingFolder = pendingWipeFolder;
        _clock = clock ?? TimeProvider.System;
        _options = options ?? new AutosaveOptions();
    }

    /// <summary>Whose drafts are open now; null when nobody is signed in.</summary>
    public DraftScope? Scope { get; private set; }

    /// <summary>The store of <see cref="Scope"/>; null when nobody is signed in.</summary>
    public IDraftStore? Store { get; private set; }

    /// <summary>The coordinator of <see cref="Scope"/>; null when nobody is signed in.</summary>
    public AutosaveCoordinator? Coordinator { get; private set; }

    /// <summary>Raised after <see cref="Scope"/> changed.</summary>
    public event EventHandler? ScopeChanged;

    /// <summary>A line for the status bar (a failed wipe, drafts kept in memory only).</summary>
    public event Action<string>? Notice;

    /// <summary>Makes <paramref name="next"/> the open scope (null: nobody). The old scope is flushed to itself and disposed first.</summary>
    public async Task SwitchAsync(DraftScope? next)
    {
        await _gate.WaitAsync().ConfigureAwait(true);
        try
        {
            if (Equals(Scope, next))
            {
                return;
            }

            await CloseCurrentAsync(wipe: false).ConfigureAwait(true);
            if (next is not null)
            {
                var store = _storeFactory(next.Validated());
                Store = store;
                Scope = next;
                Coordinator = new AutosaveCoordinator(store, next, _clock, _options);
                if (store.MemoryOnlyNotice is { } memory)
                {
                    Notice?.Invoke(memory);
                }
            }

            ScopeChanged?.Invoke(this, EventArgs.Empty);
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <summary>The user signed out (or the session ended): every draft of the scope and its key go, in-flight writes are cancelled.</summary>
    public async Task SignOutAsync()
    {
        await _gate.WaitAsync().ConfigureAwait(true);
        try
        {
            await CloseCurrentAsync(wipe: true).ConfigureAwait(true);
            ScopeChanged?.Invoke(this, EventArgs.Empty);
        }
        finally
        {
            _ = _gate.Release();
        }
    }

    /// <summary>Writes every draft with unwritten changes (the app was deactivated or is closing).</summary>
    public Task FlushAllAsync() => Coordinator?.FlushAllAsync() ?? Task.CompletedTask;

    /// <summary>Removes the drafts of earlier sign-outs whose wipe did not finish. Call once at start.</summary>
    public async Task RetryPendingWipesAsync()
    {
        if (_pendingFolder is null || !Directory.Exists(_pendingFolder))
        {
            return;
        }

        foreach (var marker in Directory.EnumerateFiles(_pendingFolder, "*.wipe"))
        {
            DraftScope scope;
            try
            {
                var lines = await File.ReadAllLinesAsync(marker).ConfigureAwait(true);
                scope = new DraftScope(lines[0], lines[1]).Validated();
            }
            catch (Exception ex) when (ex is IOException or IndexOutOfRangeException or ArgumentException or UnauthorizedAccessException)
            {
                continue;
            }

            if (await WipeAsync(_storeFactory(scope), scope).ConfigureAwait(true))
            {
                TryDelete(marker);
            }
            else
            {
                Notice?.Invoke("Unsaved drafts of an earlier session could not be removed yet. They will be tried again at the next start.");
            }
        }
    }

    public void Dispose()
    {
        Coordinator?.Dispose();
        (Store as IDisposable)?.Dispose();
        Coordinator = null;
        Store = null;
        Scope = null;
        _gate.Dispose();
    }

    private async Task CloseCurrentAsync(bool wipe)
    {
        var coordinator = Coordinator;
        var store = Store;
        var scope = Scope;
        Coordinator = null;
        Store = null;
        Scope = null;
        if (coordinator is null || store is null || scope is null)
        {
            return;
        }

        if (wipe)
        {
            // Nothing is flushed: the drafts are about to go.
            await coordinator.SignOutAsync().ConfigureAwait(true);
            if (!await IsWipedAsync(store, scope).ConfigureAwait(true))
            {
                RememberPendingWipe(scope);
                Notice?.Invoke("Unsaved drafts could not be removed from this machine. They will be removed at the next start.");
            }
        }
        else
        {
            // Flushed to its own scope, then disposed, before any other scope is opened.
            await coordinator.FlushAllAsync().ConfigureAwait(true);
            coordinator.Dispose();
        }

        (store as IDisposable)?.Dispose();
    }

    private static async Task<bool> WipeAsync(IDraftStore store, DraftScope scope)
    {
        try
        {
            await store.DeleteAllAsync(scope).ConfigureAwait(true);
            return await IsWipedAsync(store, scope).ConfigureAwait(true);
        }
        finally
        {
            (store as IDisposable)?.Dispose();
        }
    }

    /// <summary>Whether the scope holds no draft. Looking makes the store create the scope's key again,
    /// so a clean result wipes once more: the key must not outlive the sign-out.</summary>
    private static async Task<bool> IsWipedAsync(IDraftStore store, DraftScope scope)
    {
        try
        {
            var empty = (await store.ListAsync(scope).ConfigureAwait(true)).Count == 0;
            await store.DeleteAllAsync(scope).ConfigureAwait(true);
            return empty;
        }
        catch (Exception ex) when (ex is not OperationCanceledException)
        {
            return false;
        }
    }

    private void RememberPendingWipe(DraftScope scope)
    {
        if (_pendingFolder is null)
        {
            return;
        }

        try
        {
            _ = Directory.CreateDirectory(_pendingFolder);
            File.WriteAllLines(Path.Combine(_pendingFolder, scope.DirectoryName + ".wipe"), [scope.ServerId, scope.UserId]);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // The notice above already told the person; there is nowhere left to write the reminder.
        }
    }

    private static void TryDelete(string path)
    {
        try
        {
            File.Delete(path);
        }
        catch (Exception ex) when (ex is IOException or UnauthorizedAccessException)
        {
            // Tried again at the next start.
        }
    }
}
