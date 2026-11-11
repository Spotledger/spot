using Spotledger.Client.Metadata;

namespace Spotledger.Client.Designer;

/// <summary>
/// The preview's document: a <see cref="DocTypeModel"/> and a <see cref="DocumentDraft"/> built
/// from the server's preview metadata, filled with the sample values the server supplied. The
/// preview pane hands each field to <see cref="FieldControlFactory"/>, so what the designer sees
/// is the real form control for the type, not a look-alike. Nothing here is ever saved: the draft
/// is an in-memory board.
/// </summary>
public sealed class PreviewBoard
{
    private PreviewBoard(DocTypeModel model, DocumentDraft draft)
    {
        Model = model;
        Draft = draft;
    }

    /// <summary>The previewed DocType's model.</summary>
    public DocTypeModel Model { get; }

    /// <summary>The board's unsaved document, holding the sample values.</summary>
    public DocumentDraft Draft { get; }

    /// <summary>Builds the board from the server's preview.</summary>
    public static PreviewBoard From(PreviewModel preview)
    {
        ArgumentNullException.ThrowIfNull(preview);
        var model = BuildDocType(preview.Doctype, preview.Label, preview.FieldList, preview.ChildList);
        var draft = new DocumentDraft(model);
        foreach (var field in preview.FieldList)
        {
            if (string.IsNullOrEmpty(field.Sample) || field.Type == "table")
            {
                continue;
            }

            draft.TrySetText(field.Name, field.Sample);
        }

        return new PreviewBoard(model, draft);
    }

    /// <summary>The state of a previewed field, for the control factory.</summary>
    public FieldState? State(string name) => Draft.Fields.TryGetValue(name, out var state) ? state : null;

    /// <summary>The previewed fields in the order the form shows them, with their labels.</summary>
    public IReadOnlyList<FieldModel> FormFields => Model.Fields;

    private static DocTypeModel BuildDocType(
        string name,
        string label,
        IReadOnlyList<PreviewFieldSnapshot> fields,
        IReadOnlyList<PreviewChildSnapshot> children)
    {
        var models = new List<FieldModel>(fields.Count);
        var childTables = new Dictionary<string, DocTypeModel>(StringComparer.Ordinal);
        foreach (var field in fields)
        {
            var kind = FieldTypeCatalog.KindOf(field.Type);
            var model = new FieldModel(
                name: field.Name,
                label: field.Label,
                kind: kind,
                format: null,
                scale: null,
                precision: null,
                options: field.Options,
                linkTo: field.LinkTarget,
                linkTypeField: null,
                childDoctype: field.ChildDoctype,
                linkField: null,
                maxChars: null,
                lookup: null,
                hints: null,
                required: field.Required,
                readOnly: false,
                defaultValue: null);
            models.Add(model);
        }

        foreach (var child in children)
        {
            var childModel = BuildDocType(child.Doctype, child.Label, child.Fields ?? Array.Empty<PreviewFieldSnapshot>(), Array.Empty<PreviewChildSnapshot>());
            for (var i = 0; i < models.Count; i++)
            {
                if (string.Equals(models[i].ChildDoctype, child.Doctype, StringComparison.Ordinal))
                {
                    models[i] = WithChildTable(models[i], child.Doctype);
                    childTables[models[i].Name] = childModel;
                }
            }
        }

        return new DocTypeModel(name, label, 0, false, models, childTables);
    }

    private static FieldModel WithChildTable(FieldModel model, string childDoctype) =>
        new(
            name: model.Name,
            label: model.Label,
            kind: model.Kind,
            unknownKindName: null,
            format: null,
            scale: null,
            precision: null,
            options: model.Options,
            linkTo: null,
            linkTypeField: null,
            childDoctype: childDoctype,
            linkField: null,
            maxChars: null,
            lookup: null,
            hints: null,
            required: model.Required,
            readOnly: false,
            defaultValue: null);
}
