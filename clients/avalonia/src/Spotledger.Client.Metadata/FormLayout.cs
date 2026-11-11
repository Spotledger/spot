namespace Spotledger.Client.Metadata;

/// <summary>
/// A renderer-neutral layout snapshot: tabs, sections and their visible fields in display
/// order, plus the layout problems that would strand a save. Rebuild it after a draft
/// change; building never mutates the draft, and nothing in it knows about controls.
///
/// The native metadata contract carries no view definition in this version (ADR-068 A3.3,
/// which leaves views to the form designer), so a layout comes from one of two places:
/// an explicit <see cref="FormView"/> when the host has one (a view or site
/// customisation), otherwise the DocType's own field order — the generated default view
/// of ADR-068 §7. In both cases a field hidden by a **rule** is omitted (it is not a
/// layout error: the rule may show it again), while a mandatory field with no value that
/// the **layout** hides is reported (<see cref="Build"/>, ADR-068 §7.5).
/// </summary>
public sealed class FormLayout
{
    private FormLayout(IReadOnlyList<FormTab> tabs, IReadOnlyList<FieldError> errors)
    {
        Tabs = tabs;
        Errors = errors;
    }

    /// <summary>The resolved tabs in display order.</summary>
    public IReadOnlyList<FormTab> Tabs { get; }

    /// <summary>Layout problems that would hide a field the user cannot leave empty.</summary>
    public IReadOnlyList<FieldError> Errors { get; }

    /// <summary>Builds the generated default layout from the DocType's field order.</summary>
    /// <param name="model">The draft's effective metadata.</param>
    /// <param name="draft">The draft whose rule state decides what is shown.</param>
    public static FormLayout Build(DocTypeModel model, DocumentDraft draft) => Build(model, draft, null);

    /// <summary>Builds the resolved layout: the view's tabs when one is given, otherwise
    /// the DocType's field order.</summary>
    /// <param name="model">The draft's effective metadata.</param>
    /// <param name="draft">The draft whose rule state decides what is shown.</param>
    /// <param name="view">The view to resolve, or null for the generated default.</param>
    public static FormLayout Build(DocTypeModel model, DocumentDraft draft, FormView? view)
    {
        ArgumentNullException.ThrowIfNull(model);
        ArgumentNullException.ThrowIfNull(draft);
        if (!ReferenceEquals(model, draft.Model))
        {
            throw new ArgumentException("The layout model must be the draft's effective metadata.", nameof(model));
        }

        if (view is null)
        {
            var fields = new List<FieldModel>();
            var errors = new List<FieldError>();
            foreach (var field in model.Fields)
            {
                if (Place(field, draft, field.Hints.Hidden, errors))
                {
                    fields.Add(field);
                }
            }

            var section = new FormSection(model.Label, fields.AsReadOnly());
            var tab = new FormTab(model.Label, Array.AsReadOnly(new[] { section }));
            return new FormLayout(Array.AsReadOnly(new[] { tab }), errors.AsReadOnly());
        }

        var inView = new HashSet<string>(StringComparer.Ordinal);
        var tabs = new List<FormTab>(view.Tabs.Count);
        var problems = new List<FieldError>();
        foreach (var tab in view.Tabs)
        {
            var sections = new List<FormSection>(tab.Sections.Count);
            foreach (var section in tab.Sections)
            {
                var placed = new List<FieldModel>();
                foreach (var name in section.Fields)
                {
                    var field = model.Field(name);
                    if (field is null)
                    {
                        problems.Add(new FieldError(name, "layout.unknown_field",
                            $"The layout places {name}, which {model.Label} does not have."));
                        continue;
                    }

                    inView.Add(field.Name);
                    if (Place(field, draft, field.Hints.Hidden, problems))
                    {
                        placed.Add(field);
                    }
                }

                sections.Add(new FormSection(section.Label, placed.AsReadOnly()));
            }

            tabs.Add(new FormTab(tab.Label, sections.AsReadOnly()));
        }

        // A field the view does not place is hidden by the view, so the same §7.5 check
        // applies to it: a required field with no value and no way to fill it.
        foreach (var field in model.Fields)
        {
            if (!inView.Contains(field.Name))
            {
                Place(field, draft, hiddenByView: true, problems);
            }
        }

        return new FormLayout(tabs.AsReadOnly(), problems.AsReadOnly());
    }

    /// <summary>
    /// Decides what a field contributes to the layout. Answers true when it is shown and
    /// must be placed; answers false either because it is hidden (by the layout, or by a
    /// rule). A hidden mandatory field with no value is the §7.5 error, but only when the
    /// **layout** hides it — a rule that hides it may show it again, and its value would
    /// be required then.
    /// </summary>
    private static bool Place(
        FieldModel field,
        DocumentDraft draft,
        bool hiddenByView,
        List<FieldError> errors)
    {
        if (!draft.Fields.TryGetValue(field.Name, out var state))
        {
            return false;
        }

        if (!hiddenByView && state.IsVisible)
        {
            return true;
        }

        if (hiddenByView && state.IsMandatory && DraftValidator.IsEmptyValue(state.Value))
        {
            errors.Add(new FieldError(field.Name, "layout.hidden_required",
                $"The layout hides required field {field.Label} without a value."));
        }

        return false;
    }
}

/// <summary>A tab and its sections in display order.</summary>
public sealed record FormTab(string Label, IReadOnlyList<FormSection> Sections);

/// <summary>A section and its visible fields in display order.</summary>
public sealed record FormSection(string Label, IReadOnlyList<FieldModel> Fields);

/// <summary>
/// A view's layout, as the host has it: tabs, sections and the field names they place, in
/// display order. Client-side and provisional — the contract carries no view schema yet
/// (ADR-068 A3.3), so a host that has no view simply omits it and gets the generated
/// default. A field the view does not place is hidden by the view.
/// </summary>
/// <param name="Tabs">The tabs, in display order.</param>
public sealed record FormView(IReadOnlyList<FormViewTab> Tabs);

/// <summary>One tab of a <see cref="FormView"/>.</summary>
/// <param name="Label">The tab's label.</param>
/// <param name="Sections">The tab's sections, in display order.</param>
public sealed record FormViewTab(string Label, IReadOnlyList<FormViewSection> Sections);

/// <summary>One section of a <see cref="FormView"/>.</summary>
/// <param name="Label">The section's label.</param>
/// <param name="Fields">The field names the section places, in display order.</param>
public sealed record FormViewSection(string Label, IReadOnlyList<string> Fields);
