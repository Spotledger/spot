using System;
using System.Collections.Generic;
using System.Globalization;
using System.Linq;
using System.Text;

namespace Spotledger.Client.FlowDesigner.Participants;

/// One node's effect badge: the worst reversibility among its declared effects, whether any of them
/// leaves the building, and the words for the badge and for a screen reader.
public sealed record NodeEffectBadge(
    string NodeId,
    EffectReversibility Worst,
    bool HasInternal,
    bool HasExternal,
    int Count,
    string Text,
    string HelpText);

/// The per-node effect badges of the designer, built from the server's <c>effect_summary</c>. The
/// server decides what a step declares and how it is undone; this type only orders and words it.
public sealed class EffectBadges
{

    private readonly Dictionary<string, NodeEffectBadge> _byNode = new(StringComparer.Ordinal);

    public EffectBadges(IEnumerable<NodeEffectsInfo>? summary)
    {
        foreach (NodeEffectsInfo node in summary ?? [])
        {
            if (node.Effects.Count == 0)
            {
                continue;
            }

            EffectReversibility worst = node.Effects.Max(effect => effect.Reversibility);
            bool internalEffect = node.Effects.Any(effect => effect.Place == EffectPlace.Internal);
            bool external = node.Effects.Any(effect => effect.Place == EffectPlace.External);
            _byNode[node.NodeId] = new NodeEffectBadge(
                node.NodeId,
                worst,
                internalEffect,
                external,
                node.Effects.Count,
                ParticipantStrings.BadgeText(worst, internalEffect, external),
                ParticipantStrings.BadgeHelp(node.NodeId, node.Effects));
        }
    }

    public static EffectBadges Empty { get; } = new([]);

    public IReadOnlyCollection<NodeEffectBadge> All => _byNode.Values;

    public NodeEffectBadge? Badge(string nodeId)
    {
        ArgumentNullException.ThrowIfNull(nodeId);
        return _byNode.GetValueOrDefault(nodeId);
    }
}

/// One line of the test-run effect preview.
public sealed record EffectPreviewLine(string Text, bool IsUndeclared);

/// The effects of one node in the preview.
public sealed record EffectPreviewGroup(string NodeId, string Heading, IReadOnlyList<EffectPreviewLine> Lines)
{

    public bool HasUndeclared => Lines.Any(line => line.IsUndeclared);
}

/// The test-run effect preview panel: the receipts of the run grouped by node in the order of the
/// process, an undeclared effect flagged where it happened, then the totals by class. An analyst
/// reads this before going live.
public sealed class EffectPreviewPanel
{

    public EffectPreviewPanel(FlowDocument document, EffectPreview? preview)
    {
        ArgumentNullException.ThrowIfNull(document);
        Preview = preview;
        if (preview is null)
        {
            Groups = [];
            Totals = [];
            return;
        }

        List<NodePreview> ordered = [];
        foreach (FlowNode node in document.Nodes)
        {
            ordered.AddRange(preview.Nodes.Where(entry => string.Equals(entry.NodeId, node.Id, StringComparison.Ordinal)));
        }

        // A receipt for a step the document no longer has is still shown, after the known steps.
        ordered.AddRange(preview.Nodes.Where(entry => document.Node(entry.NodeId) is null));
        Groups =
        [
            .. ordered
                .Where(entry => entry.Effects.Count > 0)
                .Select(entry => new EffectPreviewGroup(
                    entry.NodeId,
                    entry.NodeId,
                    [.. entry.Effects.Select(effect => new EffectPreviewLine(
                        ParticipantStrings.PreviewLine(effect),
                        effect.Undeclared is not null))])),
        ];
        Totals = [.. preview.Totals.Select(total => ParticipantStrings.TotalLine(total))];
    }

    public EffectPreview? Preview { get; }

    public IReadOnlyList<EffectPreviewGroup> Groups { get; }

    public IReadOnlyList<string> Totals { get; }

    public long UndeclaredCount => Preview?.Undeclared ?? 0;

    public bool HasUndeclared => UndeclaredCount > 0 || Groups.Any(group => group.HasUndeclared);

    public string Heading => ParticipantStrings.PreviewHeading(Groups.Count, UndeclaredCount);

    public string ToText()
    {
        StringBuilder builder = new();
        builder.Append(Heading).Append('\n');
        foreach (EffectPreviewGroup group in Groups)
        {
            builder.Append(group.Heading).Append(":\n");
            foreach (EffectPreviewLine line in group.Lines)
            {
                builder.Append("  ").Append(line.Text).Append('\n');
            }
        }

        foreach (string total in Totals)
        {
            builder.Append(total).Append('\n');
        }

        return builder.ToString();
    }

    /// The count as a plain integer string, for tests and screen readers.
    public static string Number(long value) => value.ToString(CultureInfo.InvariantCulture);
}
