using System;
using System.Collections.Generic;
using System.Linq;
using Avalonia;

namespace Spotledger.Client.FlowDesigner.Participants;

/// Which way a message crosses a lane boundary, from this process's side.
public enum MessageDirection
{
    Out,
    In,
}

/// One participant lane. The process's own lane holds its steps; every other lane is only a
/// labelled band whose inside is hidden.
public sealed record LaneInfo(string Role, ParticipantKind Kind, bool IsOwn);

/// A message step and the lane on the other end of it. <see cref="Known"/> is false when the contract
/// does not define the message, or defines it for the other direction, so the canvas can mark the
/// connection instead of inventing a partner for it.
public sealed record LaneMessageLink(
    string NodeId,
    string Message,
    string Role,
    MessageDirection Direction,
    bool Known);

/// One band of the lane drawing, in designer space.
public sealed record LaneBand(string Role, ParticipantKind Kind, bool IsOwn, Rect Bounds, bool Above);

/// The participant lanes of a process: the role this process plays (highlighted) and every other
/// role of the contract as a labelled lane with hidden insides. Messages are the only connections
/// across lanes: a flow edge always joins two steps of the own lane, so the only things that reach
/// another lane are the message links this type lists.
public sealed class ParticipantLanes
{

    public const double BandHeight = 44;

    public const double BandGap = 14;

    public const double Margin = 36;

    public ParticipantLanes(ProcessContract? contract, string ownRole)
    {
        ArgumentNullException.ThrowIfNull(ownRole);
        Contract = contract;
        OwnRole = ownRole;
        ParticipantKind ownKind = contract?.Participant(ownRole)?.Kind ?? ParticipantKind.Tenant;
        Own = new LaneInfo(ownRole, ownKind, true);
        Others = contract is null
            ? []
            : [.. contract.OtherParticipants(ownRole).Select(p => new LaneInfo(p.Role, p.Kind, false))];
    }

    public ProcessContract? Contract { get; }

    public string OwnRole { get; }

    public LaneInfo Own { get; }

    public IReadOnlyList<LaneInfo> Others { get; }

    public LaneInfo? Lane(string role)
    {
        ArgumentNullException.ThrowIfNull(role);
        return string.Equals(role, OwnRole, StringComparison.Ordinal)
            ? Own
            : Others.FirstOrDefault(lane => string.Equals(lane.Role, role, StringComparison.Ordinal));
    }

    /// The message links of the document: one per send or wait step that names a message, in node
    /// order. The role on the far end comes from the contract.
    public IReadOnlyList<LaneMessageLink> Links(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        List<LaneMessageLink> links = [];
        foreach (FlowNode node in document.Nodes)
        {
            bool send = string.Equals(node.Kind, ParticipantKinds.SendMessage, StringComparison.Ordinal);
            bool wait = string.Equals(node.Kind, ParticipantKinds.WaitMessage, StringComparison.Ordinal);
            if (!send && !wait)
            {
                continue;
            }

            string? name = (node.ConfigValue(ParticipantKinds.MessageKey) as FlowStringValue)?.Value;
            if (string.IsNullOrEmpty(name))
            {
                continue;
            }

            ContractMessage? message = Contract?.Message(name);
            MessageDirection direction = send ? MessageDirection.Out : MessageDirection.In;
            bool known = message is not null
                && (send
                    ? string.Equals(message.From, OwnRole, StringComparison.Ordinal)
                    : string.Equals(message.To, OwnRole, StringComparison.Ordinal));
            string role = message is null ? string.Empty : send ? message.To : message.From;
            links.Add(new LaneMessageLink(node.Id, name, role, direction, known && Lane(role) is not null));
        }

        return links;
    }

    /// The steps that exchange a message with one lane.
    public IReadOnlyList<LaneMessageLink> LinksOf(FlowDocument document, string role)
    {
        ArgumentNullException.ThrowIfNull(role);
        return [.. Links(document).Where(link => string.Equals(link.Role, role, StringComparison.Ordinal))];
    }

    /// The bands of the drawing around the given node bounds: the own lane wraps the steps and
    /// the other lanes stack above and below it, alternately, so no message line has to cross a
    /// third lane's band more than needed.
    public IReadOnlyList<LaneBand> Bands(Rect steps)
    {
        double left = steps.X - Margin;
        double width = Math.Max(steps.Width + (2 * Margin), 360);
        Rect own = new(left, steps.Y - Margin, width, Math.Max(steps.Height + (2 * Margin), 120));
        List<LaneBand> bands = [new LaneBand(Own.Role, Own.Kind, true, own, false)];
        int above = 0;
        int below = 0;
        for (int i = 0; i < Others.Count; i++)
        {
            LaneInfo lane = Others[i];
            bool up = i % 2 == 0;
            double y = up
                ? own.Top - ((BandHeight + BandGap) * (above + 1))
                : own.Bottom + BandGap + ((BandHeight + BandGap) * below);
            if (up)
            {
                above++;
            }
            else
            {
                below++;
            }

            bands.Add(new LaneBand(lane.Role, lane.Kind, false, new Rect(left, y, width, BandHeight), up));
        }

        return bands;
    }

    /// Where a message line meets a lane: straight above or below the step, on the near edge of the
    /// band, clamped to the band's width.
    public static Point Port(LaneBand band, Rect step)
    {
        double x = Math.Clamp(step.Center.X, band.Bounds.Left + 8, band.Bounds.Right - 8);
        return new Point(x, band.Above ? band.Bounds.Bottom : band.Bounds.Top);
    }
}
