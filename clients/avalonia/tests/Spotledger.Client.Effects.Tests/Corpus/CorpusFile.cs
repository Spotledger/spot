using Spotledger.Client.Effects.Tests.Toml;

namespace Spotledger.Client.Effects.Tests.Corpus;

/// <summary>One case of one corpus file, with the fragments it inherits resolved.</summary>
internal sealed class CorpusCase
{
    private CorpusCase(TomlTable raw, CorpusFile file)
    {
        Id = raw.RequireString("id");
        Description = raw.String("description") ?? string.Empty;
        Meta = raw.Table("meta");
        Children = ReadChildren(raw.Table("children"));
        Doc = raw.Table("doc") ?? TomlTable.Empty;
        Event = raw.RequireTable("event");
        Scripts = raw.Tables("script");
        Security = raw.Table("security") ?? TomlTable.Empty;
        Dialogs = raw.Strings("dialogs");
        ServerPatch = raw.Table("server_patch");
        Expect = raw.RequireTable("expect");
        File = file;
    }

    /// <summary>The case id (<c>set_value/decimal-exact</c>).</summary>
    public string Id { get; }

    /// <summary>What the case proves, in a sentence.</summary>
    public string Description { get; }

    /// <summary>The metadata the case overrides, when it does.</summary>
    public TomlTable? Meta { get; }

    /// <summary>The child DocTypes the case adds or overrides.</summary>
    public IReadOnlyDictionary<string, TomlTable> Children { get; }

    /// <summary>The document members the case overrides.</summary>
    public TomlTable Doc { get; }

    /// <summary>The event that triggers the scripts.</summary>
    public TomlTable Event { get; }

    /// <summary>The scripts the case registers.</summary>
    public IReadOnlyList<TomlTable> Scripts { get; }

    /// <summary>The security the form is served with (server-side; the client never sees it).</summary>
    public TomlTable Security { get; }

    /// <summary>The dialogs the form registers (server-side).</summary>
    public IReadOnlyList<string> Dialogs { get; }

    /// <summary>The endpoint's own patch, when the case has one.</summary>
    public TomlTable? ServerPatch { get; }

    /// <summary>What the event must answer.</summary>
    public TomlTable Expect { get; }

    /// <summary>The file the case belongs to.</summary>
    public CorpusFile File { get; }

    /// <summary>The effects the server sends the client, as JSON.</summary>
    public string EffectsJson =>
        "{\"version\":1,\"effects\":" + TomlJson.Write(Expect.ArrayOf("effects") ?? new List<object?>()) + "}";

    /// <summary>The scripts' patch, as JSON object text (the server's diff adds its own members).</summary>
    public string? PatchJson => Expect.Table("patch") is { } patch ? TomlJson.Write(patch) : null;

    /// <summary>The problems the server reports (the client never sees a dropped effect).</summary>
    public IReadOnlyList<TomlTable> Problems => Expect.Tables("problem");

    /// <summary>The error codes the event answers with.</summary>
    public IReadOnlyList<string> Errors => Expect.Strings("errors");

    /// <summary>The state snapshot the case expects, when it has one.</summary>
    public TomlTable? State => Expect.Table("state");

    internal static IReadOnlyList<CorpusCase> Read(TomlTable raw, CorpusFile file) =>
        raw.Tables("case").Select(c => new CorpusCase(c, file)).ToArray();

    private static Dictionary<string, TomlTable> ReadChildren(TomlTable? children)
    {
        var map = new Dictionary<string, TomlTable>(StringComparer.Ordinal);
        if (children is null)
        {
            return map;
        }

        foreach (KeyValuePair<string, object?> member in children.Members)
        {
            if (TomlTable.From(member.Value) is { } table)
            {
                map[member.Key] = table;
            }
        }

        return map;
    }
}

/// <summary>One corpus file: the shared fragments and its cases.</summary>
internal sealed class CorpusFile
{
    private CorpusFile(string path, TomlTable root)
    {
        Path = path;
        FileName = System.IO.Path.GetFileName(path);
        Meta = root.Table("meta");
        Children = ReadChildren(root.Table("children"));
        Doc = root.Table("doc") ?? TomlTable.Empty;
        Cases = CorpusCase.Read(root, this);
    }

    /// <summary>The file's full path.</summary>
    public string Path { get; }

    /// <summary>The file's name (the case's file in a failure message).</summary>
    public string FileName { get; }

    /// <summary>The shared metadata fragment, when the file has one.</summary>
    public TomlTable? Meta { get; }

    /// <summary>The shared child DocTypes.</summary>
    public IReadOnlyDictionary<string, TomlTable> Children { get; }

    /// <summary>The shared document.</summary>
    public TomlTable Doc { get; }

    /// <summary>The file's cases, in file order.</summary>
    public IReadOnlyList<CorpusCase> Cases { get; }

    /// <summary>Loads one corpus file.</summary>
    public static CorpusFile Load(string path) =>
        new(path, new TomlTable(Spotledger.Client.Effects.Tests.Toml.TomlParser.Parse(File.ReadAllText(path))));

    /// <summary>Loads every file of the corpus, sorted by name so failures are stable.</summary>
    public static IReadOnlyList<CorpusFile> LoadAll()
    {
        string[] files = Directory.GetFiles(CorpusDirectory(), "*.toml");
        Array.Sort(files, StringComparer.Ordinal);
        var loaded = new List<CorpusFile>(files.Length);
        foreach (string file in files)
        {
            loaded.Add(Load(file));
        }

        return loaded;
    }

    /// <summary>The corpus of the Rust crate, found by walking up from the test assembly.</summary>
    public static string CorpusDirectory()
    {
        DirectoryInfo? directory = new(AppContext.BaseDirectory);
        while (directory is not null)
        {
            string candidate = System.IO.Path.Combine(
                directory.FullName, "crates", "framework", "spot-form-effects", "conformance", "forms");
            if (Directory.Exists(candidate) && Directory.Exists(System.IO.Path.Combine(directory.FullName, "clients")))
            {
                return candidate;
            }

            directory = directory.Parent;
        }

        throw new InvalidOperationException("the effects conformance corpus is not found relative to the test assembly");
    }

    private static Dictionary<string, TomlTable> ReadChildren(TomlTable? children)
    {
        var map = new Dictionary<string, TomlTable>(StringComparer.Ordinal);
        if (children is null)
        {
            return map;
        }

        foreach (KeyValuePair<string, object?> member in children.Members)
        {
            if (TomlTable.From(member.Value) is { } table)
            {
                map[member.Key] = table;
            }
        }

        return map;
    }
}
