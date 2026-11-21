using System.Globalization;
using System.Text;
using System.Text.Json;
using Spotledger.Client.Effects.Tests.Toml;
using Spotledger.Client.Metadata;

namespace Spotledger.Client.Effects.Tests.Corpus;

/// <summary>
/// One corpus case built into the client's own objects: the metadata model, the document the
/// case describes (with the ids the Rust crate derives), the form state, the batch the server
/// would send and the diff its response carries. Building it here — and not in Rust — is what
/// makes the corpus a test of the <em>client</em>.
/// </summary>
internal sealed class CorpusFixture
{
    private CorpusFixture(
        DocTypeModel model,
        DocumentDraft draft,
        FormState state,
        EffectBatch batch,
        string diffJson,
        IReadOnlyList<EffectProblem> readProblems)
    {
        Model = model;
        Draft = draft;
        State = state;
        Batch = batch;
        DiffJson = diffJson;
        ReadProblems = readProblems;
    }

    /// <summary>The DocType model the case's metadata fragment describes.</summary>
    public DocTypeModel Model { get; }

    /// <summary>The document draft, loaded with the case's document.</summary>
    public DocumentDraft Draft { get; }

    /// <summary>The form state the round applies to.</summary>
    public FormState State { get; }

    /// <summary>The batch the server would send for the case's expected effects.</summary>
    public EffectBatch Batch { get; }

    /// <summary>The diff the response carries: the endpoint's patch overridden by the scripts'.</summary>
    public string DiffJson { get; }

    /// <summary>Every problem the wire reader met (a conformance failure when it is not empty).</summary>
    public IReadOnlyList<EffectProblem> ReadProblems { get; }

    /// <summary>Builds the case's client-side objects.</summary>
    public static CorpusFixture Build(CorpusFile file, CorpusCase testCase)
    {
        ArgumentNullException.ThrowIfNull(file);
        ArgumentNullException.ThrowIfNull(testCase);
        TomlTable meta = testCase.Meta ?? file.Meta
            ?? throw new FormatException(string.Create(
                CultureInfo.InvariantCulture, $"{file.FileName} {testCase.Id}: the file or the case provides metadata"));

        var children = new Dictionary<string, TomlTable>(file.Children, StringComparer.Ordinal);
        foreach (KeyValuePair<string, TomlTable> child in testCase.Children)
        {
            children[child.Key] = child.Value;
        }

        DocTypeModel model = BuildModel(meta, children);
        string recordId = StableIds.For(testCase.Id);
        string documentJson = DocumentJson(recordId, file.Doc, testCase.Doc);
        var draft = new DocumentDraft(model, recordId);
        using (JsonDocument document = JsonDocument.Parse(documentJson))
        {
            draft.LoadFromJson(document.RootElement);
        }

        EffectReadResult read = EffectWire.Read(testCase.EffectsJson);
        return new CorpusFixture(
            model,
            draft,
            new FormState(draft),
            read.Batch,
            DiffOf(testCase),
            read.Problems);
    }

    /// <summary>Applies the case's round through the session and answers what it did.</summary>
    public ApplyResult ApplyRound()
    {
        var session = new EffectSession(State);
        PendingEvent pending = session.BeginEvent();
        return session.ApplyRound(new EventRound(pending.Sequence, DiffJson, Batch));
    }

    private static string DiffOf(CorpusCase testCase)
    {
        var members = new Dictionary<string, object?>(StringComparer.Ordinal);
        if (testCase.ServerPatch is { } server)
        {
            foreach (KeyValuePair<string, object?> member in server.Members)
            {
                members[member.Key] = member.Value;
            }
        }

        if (testCase.Expect.Table("patch") is { } script)
        {
            foreach (KeyValuePair<string, object?> member in script.Members)
            {
                members[member.Key] = member.Value;
            }
        }

        return TomlJson.Write(members);
    }

    /// <summary>
    /// The document as JSON: the file's members overridden by the case's, with the document id
    /// and each embedded row's id derived the way the Rust crate derives them.
    /// </summary>
    private static string DocumentJson(string recordId, TomlTable fileDoc, TomlTable caseDoc)
    {
        var members = new Dictionary<string, object?>(StringComparer.Ordinal);
        foreach (KeyValuePair<string, object?> member in fileDoc.Members)
        {
            members[member.Key] = member.Value;
        }

        foreach (KeyValuePair<string, object?> member in caseDoc.Members)
        {
            members[member.Key] = member.Value;
        }

        string[] names = members.Keys.OrderBy(name => name, StringComparer.Ordinal).ToArray();
        int ordinal = 0;
        using var stream = new MemoryStream();
        using (var writer = new Utf8JsonWriter(stream))
        {
            writer.WriteStartObject();
            writer.WriteString("id", recordId);
            foreach (string name in names)
            {
                writer.WritePropertyName(name);
                if (members[name] is List<object?> rows && rows.Count > 0 && rows.All(row => row is Dictionary<string, object?>))
                {
                    writer.WriteStartArray();
                    foreach (object? row in rows)
                    {
                        writer.WriteStartObject();
                        writer.WriteString("id", StableIds.RowId(recordId, ordinal));
                        ordinal++;
                        foreach (KeyValuePair<string, object?> cell in (Dictionary<string, object?>)row!)
                        {
                            writer.WritePropertyName(cell.Key);
                            TomlJson.WriteNodeTo(writer, cell.Value);
                        }

                        writer.WriteEndObject();
                    }

                    writer.WriteEndArray();
                    continue;
                }

                TomlJson.WriteNodeTo(writer, members[name]);
            }

            writer.WriteEndObject();
        }

        return Encoding.UTF8.GetString(stream.ToArray());
    }

    private static DocTypeModel BuildModel(TomlTable meta, IReadOnlyDictionary<string, TomlTable> children)
    {
        var childModels = new Dictionary<string, DocTypeModel>(StringComparer.Ordinal);
        foreach (KeyValuePair<string, TomlTable> child in children)
        {
            string label = child.Value.String("label") ?? child.Key;
            childModels[child.Key] = new DocTypeModel(child.Key, label, 0, false, BuildFields(child.Value));
        }

        IReadOnlyList<FieldModel> fields = BuildFields(meta);
        var tables = new Dictionary<string, DocTypeModel>(StringComparer.Ordinal);
        foreach (FieldModel field in fields)
        {
            if (field.Kind == FieldKind.Table
                && field.ChildDoctype is { } doctype
                && childModels.TryGetValue(doctype, out DocTypeModel? row))
            {
                tables[field.Name] = row;
            }
        }

        string name = meta.RequireString("name");
        return new DocTypeModel(name, meta.String("label") ?? name, 0, false, fields, tables);
    }

    private static List<FieldModel> BuildFields(TomlTable meta)
    {
        var fields = new List<FieldModel>();
        foreach (TomlTable field in meta.Tables("fields"))
        {
            string name = field.RequireString("name");
            string kindName = field.RequireString("kind");
            bool multiple = field.Boolean("multiple");
            FieldKind kind = kindName switch
            {
                "bool" => FieldKind.Bool,
                "int" => FieldKind.IntegerNumber,
                "decimal" => FieldKind.DecimalNumber,
                "text" => FieldKind.Text,
                "long_text" => FieldKind.LongText,
                "select" => FieldKind.Select,
                "date" => FieldKind.Date,
                "time" => FieldKind.Time,
                "datetime" => FieldKind.Datetime,
                "link" => FieldKind.Link,
                "dynamic_link" => FieldKind.DynamicLink,
                "json" => FieldKind.Json,
                "attach" => FieldKind.Attach,
                "secret" => FieldKind.Secret,
                "table" or "table_multiselect" => FieldKind.Table,
                _ => throw new FormatException(string.Create(
                    CultureInfo.InvariantCulture, $"the corpus names an unknown field kind `{kindName}`")),
            };

            int? scale = kind == FieldKind.DecimalNumber ? PositiveInt(field, "scale", 2) : null;
            int? precision = kind == FieldKind.DecimalNumber ? PositiveInt(field, "precision", 18) : null;
            int? maxChars = field.Has("max_chars") ? (int)field.Integer("max_chars") : null;
            fields.Add(new FieldModel(
                name: name,
                label: name,
                kind: kind,
                scale: scale,
                precision: precision,
                options: field.Strings("values"),
                linkTo: field.String("doctype"),
                linkTypeField: field.String("type_field"),
                childDoctype: field.String("doctype"),
                linkField: field.String("link_field"),
                maxChars: maxChars,
                required: field.Boolean("required"),
                readOnly: field.Boolean("read_only"),
                multiple: multiple));
        }

        return fields;
    }

    private static int PositiveInt(TomlTable table, string key, int fallback)
    {
        long value = table.Integer(key, 0);
        return value <= 0 ? fallback : (int)value;
    }
}
