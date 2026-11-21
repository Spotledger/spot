using System.Text.Json;

namespace Spotledger.Client.Browser.Shell;

/// <summary>
/// The module map of a build: module id to the assembly files that make it up (<c>modules.json</c>,
/// version 1: <c>{"version":1,"modules":{"forms":["_framework/Spotledger.Client.Forms.dll"]}}</c>). Parsing
/// fails closed. There is no way to name a file that is not in the shell manifest, and no way to add a
/// module at run time: third-party assembly loading is not supported (SPK-08).
/// </summary>
public sealed class ModuleMap
{
    private readonly Dictionary<string, IReadOnlyList<string>> _modules;

    /// <summary>Creates a map.</summary>
    public ModuleMap(IReadOnlyDictionary<string, IReadOnlyList<string>> modules)
    {
        _modules = new Dictionary<string, IReadOnlyList<string>>(modules, StringComparer.Ordinal);
    }

    /// <summary>The module ids, ordinal order.</summary>
    public IReadOnlyList<string> Ids => _modules.Keys.OrderBy(k => k, StringComparer.Ordinal).ToList();

    /// <summary>The assembly paths of a module, or null when the id is unknown.</summary>
    public IReadOnlyList<string>? AssembliesOf(string id) => _modules.GetValueOrDefault(id);

    /// <summary>True when <paramref name="id"/> is a plain module id: lower-case letters, digits, '.', '-', '_'.</summary>
    public static bool IsValidId(string id)
    {
        if (id.Length is 0 or > 64)
        {
            return false;
        }

        foreach (char c in id)
        {
            if (!(c is >= 'a' and <= 'z' or >= '0' and <= '9' or '.' or '-' or '_'))
            {
                return false;
            }
        }

        return true;
    }

    /// <summary>Parses and validates a module map.</summary>
    /// <exception cref="ShellIntegrityException">The map is not acceptable.</exception>
    public static ModuleMap Parse(string json)
    {
        try
        {
            using var document = JsonDocument.Parse(json);
            var root = document.RootElement;
            if (root.ValueKind != JsonValueKind.Object
                || !root.TryGetProperty("version", out var version) || !version.TryGetInt32(out int number) || number != 1
                || !root.TryGetProperty("modules", out var modules) || modules.ValueKind != JsonValueKind.Object)
            {
                throw new ShellIntegrityException("The module map is malformed or has an unknown version.");
            }

            var result = new Dictionary<string, IReadOnlyList<string>>(StringComparer.Ordinal);
            foreach (var module in modules.EnumerateObject())
            {
                if (!IsValidId(module.Name) || module.Value.ValueKind != JsonValueKind.Array)
                {
                    throw new ShellIntegrityException("The module map has a bad module id: " + module.Name.Replace('\n', ' '));
                }

                var paths = new List<string>();
                foreach (var path in module.Value.EnumerateArray())
                {
                    string text = path.ValueKind == JsonValueKind.String ? path.GetString() ?? string.Empty : string.Empty;
                    if (!ShellManifest.IsSafePath(text) || paths.Contains(text, StringComparer.Ordinal))
                    {
                        throw new ShellIntegrityException("The module map has a bad assembly path in " + module.Name + ".");
                    }

                    paths.Add(text);
                }

                result[module.Name] = paths;
            }

            return new ModuleMap(result);
        }
        catch (JsonException)
        {
            throw new ShellIntegrityException("The module map is not valid JSON.");
        }
    }
}

/// <summary>The result of loading a module.</summary>
/// <param name="Loaded">True when the module is loaded.</param>
/// <param name="Error">A plain error message when not; null when loaded. Safe to show to the user.</param>
public sealed record ModuleLoadResult(bool Loaded, string? Error)
{
    /// <summary>The module is loaded.</summary>
    public static ModuleLoadResult Success { get; } = new(true, null);

    /// <summary>The load failed with <paramref name="error"/>.</summary>
    public static ModuleLoadResult Failure(string error) => new(false, error);
}

/// <summary>
/// The lazy module registry: a module (a form, a list, a report) is loaded the first time something opens it,
/// and only from the assemblies the module map lists for it, each of which must also be in the shell manifest of
/// the running build. Concurrent callers share one load. A failed load reports a plain error and is forgotten, so
/// the next call retries. A loaded module stays loaded.
/// </summary>
public sealed class LazyModuleRegistry
{
    private readonly ShellManifest _manifest;
    private readonly ModuleMap _modules;
    private readonly IModuleLoader _loader;
    private readonly object _gate = new();
    private readonly Dictionary<string, Task<ModuleLoadResult>> _inFlight = new(StringComparer.Ordinal);
    private readonly HashSet<string> _loaded = new(StringComparer.Ordinal);

    /// <summary>Creates the registry for one build.</summary>
    public LazyModuleRegistry(ShellManifest manifest, ModuleMap modules, IModuleLoader loader)
    {
        _manifest = manifest;
        _modules = modules;
        _loader = loader;
    }

    /// <summary>True when the module is loaded.</summary>
    public bool IsLoaded(string moduleId)
    {
        lock (_gate)
        {
            return _loaded.Contains(moduleId);
        }
    }

    /// <summary>Loads the module (or returns at once when it is loaded).</summary>
    public Task<ModuleLoadResult> LoadAsync(string moduleId, CancellationToken cancellationToken = default)
    {
        lock (_gate)
        {
            if (_loaded.Contains(moduleId))
            {
                return Task.FromResult(ModuleLoadResult.Success);
            }

            if (_inFlight.TryGetValue(moduleId, out var running))
            {
                return running;
            }

            var started = RunAsync(moduleId, cancellationToken);
            _inFlight[moduleId] = started;
            return started;
        }
    }

    private async Task<ModuleLoadResult> RunAsync(string moduleId, CancellationToken cancellationToken)
    {
        // Yield first so the caller's lock is released before the loader runs.
        await Task.Yield();
        ModuleLoadResult result;
        try
        {
            result = await LoadCoreAsync(moduleId, cancellationToken).ConfigureAwait(false);
        }
        catch (ShellNetworkException)
        {
            result = ModuleLoadResult.Failure("The module '" + moduleId + "' could not be downloaded. Check the connection and try again.");
        }
        catch (ShellIntegrityException)
        {
            result = ModuleLoadResult.Failure("The module '" + moduleId + "' failed its integrity check and was not loaded.");
        }
        catch (OperationCanceledException)
        {
            result = ModuleLoadResult.Failure("Loading the module '" + moduleId + "' was cancelled.");
        }
        catch (Exception ex) when (ex is IOException or BadImageFormatException or InvalidOperationException)
        {
            result = ModuleLoadResult.Failure("The module '" + moduleId + "' could not be loaded.");
        }

        lock (_gate)
        {
            _inFlight.Remove(moduleId);
            if (result.Loaded)
            {
                _loaded.Add(moduleId);
            }
        }

        return result;
    }

    private async Task<ModuleLoadResult> LoadCoreAsync(string moduleId, CancellationToken cancellationToken)
    {
        var paths = ModuleMap.IsValidId(moduleId) ? _modules.AssembliesOf(moduleId) : null;
        if (paths is null)
        {
            return ModuleLoadResult.Failure("The module '" + SafeName(moduleId) + "' is not part of this application.");
        }

        var entries = new List<ShellEntry>();
        foreach (string path in paths)
        {
            var entry = _manifest.Find(path);
            if (entry is null)
            {
                return ModuleLoadResult.Failure("The module '" + moduleId + "' is not available in this build.");
            }

            entries.Add(entry);
        }

        await _loader.LoadAsync(entries, cancellationToken).ConfigureAwait(false);
        return ModuleLoadResult.Success;
    }

    private static string SafeName(string id) => id.Length > 64 ? id[..64] : id.Replace('\n', ' ');
}
