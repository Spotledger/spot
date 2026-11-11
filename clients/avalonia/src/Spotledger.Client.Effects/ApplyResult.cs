namespace Spotledger.Client.Effects;

/// <summary>
/// What applying one batch did: whether the batch was applied at all, every effect it had to
/// drop and why, the kinds it applied in order, the paths it changed, and the paths that are
/// dirty afterwards.
/// </summary>
public sealed class ApplyResult
{
    private ApplyResult(
        bool applied,
        bool superseded,
        long? supersededSequence,
        IReadOnlyList<EffectProblem> problems,
        IReadOnlyList<string> appliedKinds,
        IReadOnlyList<string> changedPaths,
        IReadOnlyList<string> dirtyPaths,
        IReadOnlyList<string> preservedLocalEdits)
    {
        Applied = applied;
        Superseded = superseded;
        SupersededSequence = supersededSequence;
        Problems = problems;
        AppliedKinds = appliedKinds;
        ChangedPaths = changedPaths;
        DirtyPaths = dirtyPaths;
        PreservedLocalEdits = preservedLocalEdits;
    }

    /// <summary>False when the batch was refused whole, so nothing was applied.</summary>
    public bool Applied { get; }

    /// <summary>True when a newer event round had already been applied and this one was discarded.</summary>
    public bool Superseded { get; }

    /// <summary>The discarded round's sequence, when it was discarded.</summary>
    public long? SupersededSequence { get; }

    /// <summary>Every problem met while applying, in the order the effects were met.</summary>
    public IReadOnlyList<EffectProblem> Problems { get; }

    /// <summary>The kinds applied, in application order (a dropped effect is absent).</summary>
    public IReadOnlyList<string> AppliedKinds { get; }

    /// <summary>The paths the batch changed, in the order it changed them.</summary>
    public IReadOnlyList<string> ChangedPaths { get; }

    /// <summary>The paths that differ from the document's saved values afterwards.</summary>
    public IReadOnlyList<string> DirtyPaths { get; }

    /// <summary>The user's edits kept across a round because the server did not override them.</summary>
    public IReadOnlyList<string> PreservedLocalEdits { get; }

    /// <summary>Whether any effect was dropped.</summary>
    public bool HasProblems => Problems.Count > 0;

    /// <summary>Whether anything at all changed.</summary>
    public bool ChangedAnything => ChangedPaths.Count > 0 || AppliedKinds.Count > 0 || PreservedLocalEdits.Count > 0;

    /// <summary>A batch refused whole (an unknown version, or a body that is not a batch).</summary>
    internal static ApplyResult Refused(IReadOnlyList<EffectProblem> problems) =>
        new(false, false, null, problems, Array.Empty<string>(), Array.Empty<string>(), Array.Empty<string>(),
            Array.Empty<string>());

    /// <summary>A round discarded because a newer one was already applied.</summary>
    internal static ApplyResult Discarded(long sequence) =>
        new(false, true, sequence, Array.Empty<EffectProblem>(), Array.Empty<string>(), Array.Empty<string>(),
            Array.Empty<string>(), Array.Empty<string>());

    /// <summary>Collects the effects of one apply pass.</summary>
    internal sealed class Builder
    {
        private readonly List<EffectProblem> _problems = new();
        private readonly List<string> _appliedKinds = new();
        private readonly List<string> _changedPaths = new();
        private readonly List<string> _preserved = new();

        /// <summary>The problems met so far.</summary>
        public IReadOnlyList<EffectProblem> Problems => _problems;

        /// <summary>The kinds applied so far.</summary>
        public IReadOnlyList<string> AppliedKinds => _appliedKinds;

        /// <summary>The paths changed so far.</summary>
        public IReadOnlyList<string> ChangedPaths => _changedPaths;

        /// <summary>Records a dropped effect or another problem.</summary>
        public void Problem(EffectProblem problem) => _problems.Add(problem);

        /// <summary>Records an applied effect that changed no field path (an action, a dialog).</summary>
        public void Applied(string kind) => _appliedKinds.Add(kind);

        /// <summary>Records an applied effect and the path it changed.</summary>
        public void Applied(string kind, string path)
        {
            _appliedKinds.Add(kind);
            _changedPaths.Add(path);
        }

        /// <summary>Records a user edit the round kept.</summary>
        public void Preserved(string path) => _preserved.Add(path);

        /// <summary>Merges another pass's problems, kinds and changed paths (the effects of a round).</summary>
        public void Merge(ApplyResult other)
        {
            ArgumentNullException.ThrowIfNull(other);
            _problems.AddRange(other.Problems);
            _appliedKinds.AddRange(other.AppliedKinds);
            _changedPaths.AddRange(other.ChangedPaths);
        }

        /// <summary>Seeds the paths an earlier step of the same round already changed.</summary>
        public void SeedChanged(IEnumerable<string> paths) => _changedPaths.AddRange(paths);

        /// <summary>Freezes the pass into the result the caller sees.</summary>
        public ApplyResult Build(FormState form, bool applied = true) =>
            new(applied, false, null, Array.AsReadOnly(_problems.ToArray()),
                Array.AsReadOnly(_appliedKinds.ToArray()), Array.AsReadOnly(_changedPaths.ToArray()),
                Array.AsReadOnly(form.DirtyPaths().ToArray()), Array.AsReadOnly(_preserved.ToArray()));

        /// <summary>Freezes a pass that never ran (a refused batch).</summary>
        public ApplyResult Refused() =>
            new(false, false, null, Array.AsReadOnly(_problems.ToArray()), Array.AsReadOnly(_appliedKinds.ToArray()),
                Array.AsReadOnly(_changedPaths.ToArray()), Array.Empty<string>(), Array.Empty<string>());
    }
}
