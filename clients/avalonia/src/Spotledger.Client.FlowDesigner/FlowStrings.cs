using System;
using System.Collections.Generic;
using System.Globalization;

namespace Spotledger.Client.FlowDesigner;

/// Every user-visible string this project can produce, in one place (ADR-073, ADR-080).
///
/// Most strings do not depend on anything, so they are static; the ones that name a run are
/// instance members built from <see cref="Labels"/>, which defaults to "Request"/"Requests" and
/// follows the organisation's override. The designer previews the resolved pair, so an analyst
/// sees the organisation's wording while modelling. <see cref="All"/> lists every string for the
/// ADR-080 scan test, which fails when the literal word "Case" reaches a user.
public sealed class FlowStrings
{

    public FlowStrings(RunLabels? labels = null) => Labels = labels ?? RunLabels.Defaults;

    public RunLabels Labels { get; }

    public static string Title => "Process designer";

    public static string CanvasName => "Process canvas";

    public static string PaletteName => "Node palette";

    public static string PropertiesName => "Node properties";

    public static string ProblemsName => "Problems";

    public static string MinimapName => "Overview";

    public static string VersionsName => "Versions";

    public static string EmptyCanvas =>
        "No nodes yet. Add one from the palette, or press A to search kinds.";

    public static string ReadOnly => "Read-only: your role cannot edit this process.";

    public static string RefusedReadOnly => "This process is read-only for your role.";

    public static string SaveDraft => "Save draft";

    public static string RequestPublish => "Ask for approval to publish";

    public static string DraftSaved(int version) =>
        "Draft saved as version " + version.ToString(CultureInfo.InvariantCulture) + ".";

    public static string DraftConflict => "Someone else saved a newer draft. Reload before saving again.";

    public static string PublishRequested => "Publish request sent for review.";

    public static string VersionsHeading => "Versions (read-only)";

    public static string ProblemsHeading => "Problems";

    public static string ProblemsHint => "Select a problem to focus its node.";

    public static string ProblemCount(int count) =>
        count == 1 ? "1 problem" : count.ToString(CultureInfo.InvariantCulture) + " problems";

    public static string Checking => "Checking the process with the server…";

    public static string Checked =>
        "The server is the authority on this process; these problems come from it.";

    public static string PaletteSearch => "Search node kinds";

    public static string PaletteHint => "Type to search, Enter to add, Escape to close.";

    public static string NodeName(string kind, string id) =>
        string.IsNullOrEmpty(id) ? kind + " node" : kind + " node " + id;

    public static string NodeHelp(string kind, string id, int problems) =>
        kind + " node " + id + (problems == 0 ? ", no problems" : ", " + ProblemCount(problems));

    public static string EdgeName(string from, string to) =>
        "Connection from " + from + " to " + to;

    public static string EdgeHelp(string from, string to, string? condition) =>
        string.IsNullOrEmpty(condition)
            ? EdgeName(from, to)
            : "Connection from " + from + " to " + to + " when " + condition;

    public static string Selected(int count) =>
        count == 1 ? "1 node selected" : count.ToString(CultureInfo.InvariantCulture) + " nodes selected";

    public static string ConnectPrompt(string from) =>
        "Connecting from " + from + ". Choose a target with Tab or the arrow keys, Enter to connect, Escape to cancel.";

    public static string ConnectRefused(string reason) => "Cannot connect: " + reason;

    public static string Undo => "Undo";

    public static string Redo => "Redo";

    public static string Delete => "Delete";

    public static string Copy => "Copy";

    public static string Paste => "Paste";

    public static string AutoLayout => "Auto-layout";

    public static string UnknownKind(string kind) =>
        "The kind " + kind + " is newer than this build; its settings are shown as raw text and are kept as written.";

    public static string KindMissing => "That kind is not in the server's catalogue.";

    public static string IdRenamed(string from, string to) =>
        from + " was already taken, so the copy is " + to + ".";

    public static string PublishDenied => "Your role may edit this process but not ask for its publication.";

    public string RunLabelPreview =>
        "A published version is started for each new " + Labels.SingularLower + "; people see \""
        + Labels.Singular + "\" and \"" + Labels.Plural + "\" in every screen.";

    public IReadOnlyList<string> All =>
    [
        Title,
        CanvasName,
        PaletteName,
        PropertiesName,
        ProblemsName,
        MinimapName,
        VersionsName,
        EmptyCanvas,
        ReadOnly,
        RefusedReadOnly,
        RunLabelPreview,
        SaveDraft,
        RequestPublish,
        DraftSaved(3),
        DraftConflict,
        PublishRequested,
        VersionsHeading,
        ProblemsHeading,
        ProblemsHint,
        ProblemCount(2),
        Checking,
        Checked,
        PaletteSearch,
        PaletteHint,
        NodeName("Document", "create_delivery"),
        NodeHelp("Document", "create_delivery", 1),
        EdgeName("start", "first"),
        EdgeHelp("start", "first", "doc.approved == true"),
        Selected(3),
        ConnectPrompt("start"),
        ConnectRefused("the target is already connected."),
        Undo,
        Redo,
        Delete,
        Copy,
        Paste,
        AutoLayout,
        UnknownKind("send_message"),
        KindMissing,
        IdRenamed("start", "start_2"),
        PublishDenied,
    ];
}
