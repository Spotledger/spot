using System.Globalization;
using System.Text;

namespace Spotledger.Client.Designer;

/// <summary>
/// A person-readable description of an op: for the status line, the history list and the tests
/// that compare the commands the UI issued with the server-side golden sequence.
/// </summary>
public static class DesignerOpText
{
    /// <summary>A one-line summary of the op.</summary>
    public static string Describe(DesignerOp op)
    {
        ArgumentNullException.ThrowIfNull(op);
        var child = op.Child is null ? string.Empty : $" in {op.Child}";
        return op switch
        {
            AddFieldOp add => $"add {add.Type} field {add.Name}{child}",
            RenameFieldOp rename => $"rename {rename.Name} to {rename.To}{child}",
            RemoveFieldOp remove => $"remove {remove.Name}{child}",
            ChangeTypeOp change => $"change {change.Name} to {change.Type}{child}",
            SetOptionsOp options => $"choices of {options.Name}: {string.Join(" / ", options.Options)}{child}",
            SetRequiredOp required => $"{(required.Required ? "require" : "stop requiring")} {required.Name}{child}",
            SetDefaultOp defaults => defaults.Default is null ? $"clear default of {defaults.Name}{child}" : $"default of {defaults.Name}: {defaults.Default}{child}",
            ReorderFieldOp reorder => reorder.After is null ? $"move {reorder.Name} to the end{child}" : $"move {reorder.Name} after {reorder.After}{child}",
            AddChildTableOp table => $"add child table {table.Name} ({table.ChildDoctypeName})",
            AddLinkOp link => $"add link {link.Name} to {link.Target}",
            SetNamingSeriesOp naming => naming.Naming is null ? "clear the naming series" : $"naming series {naming.Naming}",
            SetPermissionsTemplateOp permissions => $"permissions template {permissions.Template}",
            AddSectionOp section => $"add section {section.Id} (\"{section.Title}\"){child}",
            AddTabOp tab => $"add tab {tab.Id} (\"{tab.Title}\"){child}",
            SetSectionColumnsOp columns => $"set section {columns.SectionId} to {columns.Columns} columns{child}",
            MoveInLayoutOp move => move.After is null ? $"put {move.Field} in {move.SectionId}{child}" : $"put {move.Field} in {move.SectionId} after {move.After}{child}",
            SetDependsOnOp depends => depends.Condition is null ? $"clear the condition on {depends.Name}{child}" : $"condition on {depends.Name}: {Describe(depends.Condition)}",
            _ => op.Op,
        };
    }

    /// <summary>A one-line summary of a visual condition, in the expression vocabulary's words.</summary>
    public static string Describe(ConditionExpression condition)
    {
        ArgumentNullException.ThrowIfNull(condition);
        if (condition.Rows.Count == 0)
        {
            return "(no rows)";
        }

        var builder = new StringBuilder();
        for (var i = 0; i < condition.Rows.Count; i++)
        {
            if (i > 0)
            {
                builder.Append(condition.Join == ConditionJoin.Or ? " or " : " and ");
            }

            builder.Append(Describe(condition.Rows[i]));
        }

        return builder.ToString();
    }

    /// <summary>A one-line summary of one condition row.</summary>
    public static string Describe(ConditionRow row)
    {
        ArgumentNullException.ThrowIfNull(row);
        var values = string.Join(", ", row.Values.Select(v => v.ToString()));
        return string.IsNullOrEmpty(values)
            ? $"doc.{row.Field} {ConditionSyntax.Label(row.Operator)}"
            : $"doc.{row.Field} {ConditionSyntax.Label(row.Operator)} {values}";
    }

    /// <summary>The confirmation token the server requires to remove a field.</summary>
    public static string DataLossToken(string doctype, string field) =>
        string.Create(CultureInfo.InvariantCulture, $"delete {doctype}.{field}");
}
