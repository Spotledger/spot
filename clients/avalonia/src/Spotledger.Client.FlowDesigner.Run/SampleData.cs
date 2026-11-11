using System;
using System.Collections.Generic;
using Spotledger.Client.FlowDesigner;

namespace Spotledger.Client.FlowDesigner.Run;

/// One hint the sample-data editor shows: a key the trigger delivers, and whether the trigger's own
/// schema asks for it.
public sealed record SampleDataHint(string Key, string Text, bool Required);

/// One thing the editor has to say about the sample data: an error that stops the test, or a hint
/// that only advises. The code is the contract; the message is what a person reads.
public sealed record SampleDataProblem(string Code, string Message, bool Hint = false, long Line = 1, long Column = 1);

/// The editor's answer: whether a test may run with this sample, and everything it wants to say.
public sealed record SampleDataCheck(bool Ok, IReadOnlyList<SampleDataProblem> Problems, IReadOnlyList<string> UnknownKeys);

/// The sample data a dry run executes against: JSON an analyst pastes (or edits), checked against
/// the trigger's own schema so the fields the trigger delivers are the ones the samples carry.
///
/// The editor holds no state but the text; every answer is computed from the text, the document and
/// the server's catalogue, so it can never drift from the process it belongs to. Nothing here reads
/// the wall clock: a trigger's clock fields are the server's business (ADR-12).
public sealed class SampleDataEditor
{

    private readonly FlowDocument _document;

    private readonly FlowKindCatalogue _catalogue;

    public SampleDataEditor(FlowDocument document, FlowKindCatalogue catalogue)
    {
        ArgumentNullException.ThrowIfNull(document);
        ArgumentNullException.ThrowIfNull(catalogue);
        _document = document;
        _catalogue = catalogue;
    }

    /// The JSON text as the person typed it. A property rather than a field, so a host can bind a
    /// text box to it directly.
    public string Text { get; set; } = "{}";

    /// The node the sample data is shaped for: the first node the server calls a start, or a node
    /// whose schema carries a <c>trigger</c> field, or the document's first node when neither does.
    public FlowNode? Trigger
    {
        get
        {
            foreach (FlowNode node in _document.Nodes)
            {
                FlowKindSchema? schema = _catalogue.Schema(node.Kind);
                if (schema is null)
                {
                    continue;
                }

                bool carriesTrigger = schema.Fields.Count > 0 && HasField(schema, "trigger");
                bool startsTheProcess = string.Equals(schema.Category, "Start", StringComparison.OrdinalIgnoreCase)
                    || string.Equals(schema.Category, "Trigger", StringComparison.OrdinalIgnoreCase);
                if (carriesTrigger || startsTheProcess)
                {
                    return node;
                }
            }

            return _document.Nodes.Count > 0 ? _document.Nodes[0] : null;
        }
    }

    /// The hints the editor lists under the box: the trigger's own fields with the server's labels,
    /// then the two keys every trigger delivers.
    public IReadOnlyList<SampleDataHint> Hints
    {
        get
        {
            List<SampleDataHint> hints = [];
            if (Trigger is { } trigger && _catalogue.Schema(trigger.Kind) is { } schema)
            {
                foreach (FlowKindField kindField in schema.Fields)
                {
                    if (string.Equals(kindField.Key, "trigger", StringComparison.Ordinal))
                    {
                        continue;
                    }

                    hints.Add(new SampleDataHint(
                        kindField.Key,
                        kindField.Required
                            ? RunStrings.HintRequired(kindField.Label)
                            : RunStrings.HintOptional(kindField.Label),
                        kindField.Required));
                }
            }

            hints.Add(new SampleDataHint("payload", RunStrings.HintPayload, Required: true));
            hints.Add(new SampleDataHint("headers", RunStrings.HintHeaders, Required: false));
            return hints;
        }
    }

    /// Whether a key is one the trigger delivers (its own fields, or the payload and its headers).
    public bool IsTriggerKey(string key)
    {
        ArgumentNullException.ThrowIfNull(key);
        if (string.Equals(key, "payload", StringComparison.Ordinal) || string.Equals(key, "headers", StringComparison.Ordinal))
        {
            return true;
        }

        foreach (SampleDataHint hint in Hints)
        {
            if (string.Equals(hint.Key, key, StringComparison.Ordinal))
            {
                return true;
            }
        }

        return false;
    }

    /// Checks the text: valid JSON, an object at the root, and every key one the trigger delivers.
    /// A key the trigger does not deliver is a hint, never an error — the sample is kept as typed,
    /// because a pack may deliver fields this build cannot name.
    public SampleDataCheck Check()
    {
        List<SampleDataProblem> problems = [];
        List<string> unknown = [];
        JsonParseResult parsed = JsonTree.Parse(Text);
        if (!parsed.Ok || parsed.Root is null)
        {
            problems.Add(new SampleDataProblem(
                "bpm.run.sample.invalid_json",
                RunStrings.SampleDataInvalidAt(parsed.Line, parsed.Column),
                Hint: false,
                parsed.Line,
                parsed.Column));
            if (parsed.Error is { } error)
            {
                problems.Add(new SampleDataProblem(
                    "bpm.run.sample.invalid_json_detail",
                    RunStrings.SampleDataReason(error),
                    Hint: true,
                    parsed.Line,
                    parsed.Column));
            }

            return new SampleDataCheck(false, problems, unknown);
        }

        if (parsed.Root.Kind != JsonKind.Map)
        {
            problems.Add(new SampleDataProblem("bpm.run.sample.not_an_object", RunStrings.SampleDataNotObject));
            return new SampleDataCheck(false, problems, unknown);
        }

        foreach (JsonTreeNode child in parsed.Root.Children)
        {
            if (IsTriggerKey(child.Name))
            {
                continue;
            }

            unknown.Add(child.Name);
            problems.Add(new SampleDataProblem(
                "bpm.run.sample.unknown_key",
                RunStrings.UnknownKey(child.Name),
                Hint: true));
        }

        foreach (SampleDataHint hint in Hints)
        {
            if (hint.Required && parsed.Root.Child(hint.Key) is null)
            {
                problems.Add(new SampleDataProblem(
                    "bpm.run.sample.missing_key",
                    hint.Text,
                    Hint: true));
            }
        }

        bool ok = true;
        foreach (SampleDataProblem problem in problems)
        {
            if (!problem.Hint)
            {
                ok = false;
            }
        }

        return new SampleDataCheck(ok, problems, unknown);
    }

    /// The first message a person should read, or null when the sample is fine.
    public string? FirstProblem()
    {
        SampleDataCheck check = Check();
        return check.Ok || check.Problems.Count == 0 ? null : check.Problems[0].Message;
    }

    /// One line a status bar can show: whether the sample may run, and what it does not recognise.
    public string Summary()
    {
        SampleDataCheck check = Check();
        if (!check.Ok)
        {
            return check.Problems.Count > 0 ? check.Problems[0].Message : RunStrings.SampleDataInvalid;
        }

        return check.UnknownKeys.Count == 0
            ? RunStrings.SampleDataAccepted
            : RunStrings.SampleDataUnknownKeys(check.UnknownKeys.Count);
    }

    private static bool HasField(FlowKindSchema schema, string key)
    {
        foreach (FlowKindField kindField in schema.Fields)
        {
            if (string.Equals(kindField.Key, key, StringComparison.Ordinal))
            {
                return true;
            }
        }

        return false;
    }
}
