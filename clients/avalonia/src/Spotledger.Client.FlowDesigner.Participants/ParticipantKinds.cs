using System;
using System.Collections.Generic;
using System.Linq;

namespace Spotledger.Client.FlowDesigner.Participants;

/// The four node kinds a two-organisation flow needs, as the designer's kind schemas: Form, Send
/// message, Wait for message and Commit point. The designer's palette, type-ahead and property
/// panels all read one <see cref="FlowKindCatalogue"/>, so the kinds join the canvas through that
/// registration point and the canvas is not forked: a kind the server already describes keeps the
/// server's schema, and a kind it does not is filled in from here with the contract's own message
/// names as the choices.
public static class ParticipantKinds
{

    public const string Form = "form";

    public const string SendMessage = "send_message";

    public const string WaitMessage = "wait_message";

    public const string CommitPoint = "commit";

    public const string ContractKey = "contract";

    public const string MessageKey = "message";

    public const string MappingKey = "mapping";

    public const string OnRejectKey = "on_reject";

    public const string OnTimeoutKey = "on_timeout";

    /// The wait's timeout edge label (<c>WaitMessageConfig::TIMEOUT_EDGE_LABEL</c>).
    public const string TimeoutEdgeLabel = "timeout";

    public static IReadOnlyList<string> All { get; } = [Form, SendMessage, WaitMessage, CommitPoint];

    public static bool IsMessageKind(string kind) =>
        string.Equals(kind, SendMessage, StringComparison.Ordinal) || string.Equals(kind, WaitMessage, StringComparison.Ordinal);

    /// The schemas of the four kinds. With a contract and the role this process plays, a send node
    /// offers the messages the role sends and a wait node the messages it receives.
    public static IReadOnlyList<FlowKindSchema> Schemas(ProcessContract? contract = null, string? ownRole = null)
    {
        IReadOnlyList<string>? sendable = Names(contract, ownRole, sent: true);
        IReadOnlyList<string>? receivable = Names(contract, ownRole, sent: false);
        FlowValue? contractId = contract is null ? null : FlowValue.Text(contract.Id);
        return
        [
            new FlowKindSchema(
                Form,
                "Form",
                "Work",
                [
                    new FlowKindField("assignee", "Who is asked", FlowFieldType.Text, Required: true,
                        Help: "A role, a user or an assignment expression."),
                    new FlowKindField("fields", "Fields", FlowFieldType.StringList, Required: true,
                        Help: "One field name per line."),
                    new FlowKindField("draft_mode", "Draft", FlowFieldType.Select, Required: true,
                        Options: ["create", "update", "collect"], DefaultValue: FlowValue.Text("collect"),
                        Help: "Create or update a draft record, or only collect data."),
                    new FlowKindField("record_type", "Record type", FlowFieldType.Text,
                        Help: "Needed unless the form only collects data."),
                ]),
            new FlowKindSchema(
                SendMessage,
                "Send message",
                "Messages",
                [
                    new FlowKindField(ContractKey, "Contract", FlowFieldType.Text, Required: true, DefaultValue: contractId),
                    MessageField("Message to send", sendable),
                    new FlowKindField(MappingKey, "Payload mapping", FlowFieldType.MultilineText,
                        Help: "One `field = expression` line per message field, or the name of a mapping."),
                    new FlowKindField(OnRejectKey, "If the partner rejects it", FlowFieldType.Select, Required: true,
                        Options: ["error_branch", "continue"], DefaultValue: FlowValue.Text("error_branch")),
                ]),
            new FlowKindSchema(
                WaitMessage,
                "Wait for message",
                "Messages",
                [
                    new FlowKindField(ContractKey, "Contract", FlowFieldType.Text, Required: true, DefaultValue: contractId),
                    MessageField("Message to wait for", receivable),
                    new FlowKindField(OnTimeoutKey, "When the time limit passes", FlowFieldType.Select, Required: true,
                        Options: ["error_branch", "timeout_edge"], DefaultValue: FlowValue.Text("error_branch"),
                        Help: "Take the error branch, or the edge labelled timeout."),
                ]),
            new FlowKindSchema(
                CommitPoint,
                "Commit point",
                "Control",
                [
                    new FlowKindField("label", "Label", FlowFieldType.Text, Help: "Short printable text."),
                ]),
        ];
    }

    /// The catalogue to give the canvas: everything the server offers, plus any of the four kinds it
    /// does not describe. A kind the server describes keeps the server's schema, so a property panel
    /// is always the server's.
    public static FlowKindCatalogue Extend(FlowKindCatalogue server, ProcessContract? contract = null, string? ownRole = null)
    {
        ArgumentNullException.ThrowIfNull(server);
        List<FlowKindSchema> schemas = [.. server.Kinds];
        foreach (FlowKindSchema ours in Schemas(contract, ownRole))
        {
            if (server.Schema(ours.Kind) is null)
            {
                schemas.Add(ours);
            }
        }

        return new FlowKindCatalogue(schemas);
    }

    private static FlowKindField MessageField(string label, IReadOnlyList<string>? names) =>
        names is { Count: > 0 }
            ? new FlowKindField(MessageKey, label, FlowFieldType.Select, Required: true, Options: names)
            : new FlowKindField(MessageKey, label, FlowFieldType.Text, Required: true);

    private static IReadOnlyList<string>? Names(ProcessContract? contract, string? ownRole, bool sent)
    {
        if (contract is null || ownRole is null)
        {
            return null;
        }

        IReadOnlyList<ContractMessage> messages = sent ? contract.SentBy(ownRole) : contract.ReceivedBy(ownRole);
        return [.. messages.Select(message => message.Name)];
    }
}
