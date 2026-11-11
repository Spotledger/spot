using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Participants.Tests;

/// The contract editor: every edit goes through the grammar, undo and redo are exact, and what is
/// exported imports back to the same contract (the file a partner is sent).
public sealed class ContractEditorTests
{

    [Fact]
    public void AContractBuiltByEditsRoundTripsThroughAnExportedFile()
    {
        ContractEditor editor = new(ProcessContract.Empty("purchase_approval"));

        Assert.True(editor.SetTitle("Purchase approval").Ok);
        Assert.True(editor.AddParticipant("buyer", ParticipantKind.Tenant).Ok);
        Assert.True(editor.AddParticipant("approver", ParticipantKind.External).Ok);
        Assert.True(editor.AddOutcome("settled", ContractOutcomeKind.Completed).Ok);
        Assert.True(editor.AddMessage("approval_request", "procurement.approval", "buyer", "approver").Ok);
        Assert.True(editor.AddField("approval_request", "amount_minor", ContractFieldKind.Whole).Ok);
        Assert.True(editor.AddField("approval_request", "memo", ContractFieldKind.Text, required: false, maxLength: 200).Ok);
        Assert.True(editor.AddMessage("approval_decision", "procurement.approval", "approver", "buyer").Ok);
        Assert.True(editor.AddField("approval_decision", "decision", ContractFieldKind.Text, maxLength: 16).Ok);
        Assert.True(editor.AddTransition("buyer", null, "approval_request", null).Ok);
        Assert.True(editor.AddTransition("approver", "approval_request", "approval_decision", null).Ok);
        Assert.True(editor.AddTransition("buyer", "approval_decision", null, "settled").Ok);
        Assert.True(editor.SetTimeout("approval_decision", "buyer", 86_400, "settled").Ok);

        string file = editor.Export();
        ContractEditor partner = new(ProcessContract.Empty("other"));
        Assert.True(partner.Import(file).Ok);

        Assert.Equal(file, partner.Export());
        Assert.Equal(2, partner.Current.Messages.Count);
        Assert.Equal(ParticipantKind.External, partner.Current.Participant("approver")!.Kind);
        Assert.True(partner.IsModified);
    }

    [Fact]
    public void AFileThatDoesNotReadChangesNothing()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        string before = editor.Export();

        ContractEditResult result = editor.Import("this is not a contract");

        Assert.False(result.Ok);
        Assert.NotNull(result.Error);
        Assert.Equal(before, editor.Export());
        Assert.False(editor.CanUndo);
    }

    [Fact]
    public void UndoAndRedoRestoreEveryEditExactly()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        string start = editor.Export();

        Assert.True(editor.AddOutcome("withdrawn", ContractOutcomeKind.Cancelled).Ok);
        Assert.True(editor.SetFieldRequired("approval_request", "memo", true).Ok);
        string edited = editor.Export();

        Assert.True(editor.Undo());
        Assert.True(editor.Undo());
        Assert.Equal(start, editor.Export());
        Assert.True(editor.Redo());
        Assert.True(editor.Redo());
        Assert.Equal(edited, editor.Export());
        Assert.False(editor.CanRedo);
    }

    [Theory]
    [InlineData("Bad Name")]
    [InlineData("1abc")]
    [InlineData("")]
    public void ABadNameIsRefused(string name)
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());

        Assert.False(editor.AddOutcome(name, ContractOutcomeKind.Failed).Ok);
        Assert.False(editor.AddParticipant(name, ParticipantKind.Tenant).Ok);
    }

    [Fact]
    public void TheGrammarOfTopicsFieldsAndReferencesIsEnforced()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());

        Assert.False(editor.AddMessage("ping", "single", "buyer", "approver").Ok);
        Assert.False(editor.AddMessage("ping", "a.b", "buyer", "buyer").Ok);
        Assert.False(editor.AddMessage("ping", "a.b", "buyer", "nobody").Ok);
        Assert.False(editor.AddMessage("approval_request", "a.b", "buyer", "approver").Ok);
        Assert.False(editor.AddField("approval_request", "Bad", ContractFieldKind.Text).Ok);
        Assert.False(editor.AddField("approval_request", "amount_minor", ContractFieldKind.Whole).Ok);
        Assert.False(editor.AddField("approval_request", "count", ContractFieldKind.Whole, maxLength: 4).Ok);
        Assert.False(editor.AddTransition("buyer", null, "nope", null).Ok);
        Assert.False(editor.AddTransition("buyer", null, "approval_request", "settled").Ok);
        Assert.False(editor.AddTransition("buyer", null, null, null).Ok);
        Assert.False(editor.SetTimeout("approval_decision", "buyer", 0).Ok);
        Assert.False(editor.CanUndo);
    }

    [Fact]
    public void ARoleThatStillExchangesAMessageCannotBeRemoved()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());

        ContractEditResult result = editor.RemoveParticipant("approver");

        Assert.False(result.Ok);
        Assert.Equal(2, editor.Current.Participants.Count);
    }

    [Fact]
    public void RemovingAMessageRemovesItsRulesAndItsWaitLimit()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());

        Assert.True(editor.RemoveMessage("approval_decision").Ok);

        Assert.Null(editor.Current.Message("approval_decision"));
        Assert.DoesNotContain(editor.Current.Order, rule => rule.After == "approval_decision" || rule.NextMessage == "approval_decision");
        Assert.Empty(editor.Current.Timeouts);
    }

    [Fact]
    public void ANewVersionRaisesTheNumberAndDropsTheSignatures()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());

        Assert.True(editor.NewVersion().Ok);

        Assert.Equal(2, editor.Current.Version);
        Assert.Empty(editor.Current.Signatures);
        Assert.Equal(1, editor.Baseline.Version);
        Assert.Single(editor.Baseline.Signatures);
    }

    [Fact]
    public void AReadOnlyEditorRefusesEveryEdit()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract()) { ReadOnly = true };

        Assert.False(editor.AddOutcome("late", ContractOutcomeKind.Failed).Ok);
        Assert.False(editor.Import(ParticipantFixtures.Contract().ToText()).Ok);
        Assert.False(editor.NewVersion().Ok);
        Assert.False(editor.CanUndo);
    }

    [Fact]
    public void ClassifiedFieldsAreFlagged()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract(), ParticipantFixtures.SecretCostCode());

        Assert.Equal([("approval_request", "cost_code")], editor.SecretFields());
        Assert.Equal(ContractFieldClass.Secret, editor.ClassOf("cost_code"));
        Assert.Equal(ContractFieldClass.Public, editor.ClassOf("memo"));
    }

    [Fact]
    public void SavingMakesTheCurrentVersionTheBaseline()
    {
        ContractEditor editor = new(ParticipantFixtures.Contract());
        Assert.True(editor.AddOutcome("withdrawn", ContractOutcomeKind.Cancelled).Ok);
        Assert.True(editor.IsModified);

        editor.MarkSaved(2);

        Assert.False(editor.IsModified);
        Assert.Equal(2, editor.Baseline.Version);
        Assert.Contains("withdrawn", editor.Baseline.Outcomes.Select(o => o.Name));
    }
}
