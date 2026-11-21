using Avalonia.Controls;

namespace Spotledger.Designer.Reports.Views;

/// <summary>The approval wizard's view: shows the step the view-model is on.</summary>
public partial class ApprovalWizardView : UserControl
{
    /// <summary>Creates the view.</summary>
    public ApprovalWizardView()
    {
        InitializeComponent();
        ApproverKindBox.ItemsSource = ApprovalWizardViewModel.ApproverKinds;
        EscalationKindBox.ItemsSource = ApprovalWizardViewModel.ApproverKinds;
        UnitBox.ItemsSource = Enum.GetValues<TimeUnit>();
        ApproverKindBox.SelectionChanged += (_, _) =>
        {
            if (Wizard is { } w && ApproverKindBox.SelectedItem is ApproverKind kind)
            {
                w.ApproverKind = kind;
            }
        };
        EscalationKindBox.SelectionChanged += (_, _) =>
        {
            if (Wizard is { } w && EscalationKindBox.SelectedItem is ApproverKind kind)
            {
                w.EscalationKind = kind;
            }
        };
        UnitBox.SelectionChanged += (_, _) =>
        {
            if (Wizard is { } w && UnitBox.SelectedItem is TimeUnit unit)
            {
                w.Unit = unit;
            }
        };
        DataContextChanged += (_, _) => Subscribe();
        DocTypeBox.TextChanged += async (_, _) =>
        {
            if (Wizard is { } w)
            {
                await w.SearchDocTypesAsync(DocTypeBox.Text ?? string.Empty);
            }
        };
        DocTypeList.SelectionChanged += (_, _) =>
        {
            if (Wizard is { } w && DocTypeList.SelectedItem is Spotledger.Client.Designer.DocTypeChoice choice && w.DocType != choice.Name)
            {
                w.DocType = choice.Name;
            }
        };
    }

    private ApprovalWizardViewModel? Wizard => DataContext as ApprovalWizardViewModel;

    private void Subscribe()
    {
        if (Wizard is not { } wizard)
        {
            return;
        }

        wizard.PropertyChanged += (_, e) =>
        {
            if (e.PropertyName == nameof(ApprovalWizardViewModel.Step))
            {
                ShowStep(wizard.Step);
            }
        };
        ApproverKindBox.SelectedItem = wizard.ApproverKind;
        EscalationKindBox.SelectedItem = wizard.EscalationKind;
        UnitBox.SelectedItem = wizard.Unit;
        ShowStep(wizard.Step);
    }

    private void ShowStep(WizardStep step)
    {
        DocumentStep.IsVisible = step == WizardStep.Document;
        ApproverStep.IsVisible = step == WizardStep.Approver;
        LimitStep.IsVisible = step == WizardStep.TimeLimit;
        OutcomesStep.IsVisible = step == WizardStep.Outcomes;
        ReviewStep.IsVisible = step == WizardStep.Review;
    }
}
