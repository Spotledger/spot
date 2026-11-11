using System.Globalization;
using Spotledger.Client.Effects.Tests.Corpus;
using Spotledger.Client.Effects.Tests.Toml;
using Xunit;

namespace Spotledger.Client.Effects.Tests;

/// <summary>
/// The shared corpus of the Rust crate
/// (<c>crates/framework/spot-form-effects/conformance/forms/*.toml</c>): every case's metadata
/// fragment, document, event, expected effects and server diff are loaded, built into this
/// client's own metadata model and draft, applied through <see cref="EffectSession"/>, and the
/// form state that results is compared with the checked-in snapshot. The Rust runner proves
/// the server half; this proves the desktop half produces the same form state from the same
/// bytes.
/// </summary>
public class ConformanceTests
{
    private static readonly IReadOnlyList<CorpusFile> Files = CorpusFile.LoadAll();

    private static readonly Dictionary<string, CorpusCase> Index = BuildIndex();

    /// <summary>Every case of every file, as (file, case id).</summary>
    public static TheoryData<string, string> AllCases
    {
        get
        {
            var data = new TheoryData<string, string>();
            foreach (CorpusFile file in Files)
            {
                foreach (CorpusCase testCase in file.Cases)
                {
                    data.Add(file.FileName, testCase.Id);
                }
            }

            return data;
        }
    }

    /// <summary>Every file's cases are all here: a corpus file that failed to load would
    /// otherwise look like a corpus that passes.</summary>
    [Fact]
    public void EveryCorpusFileAndCaseIsLoaded()
    {
        Assert.Equal(10, Files.Count);
        int total = Files.Sum(file => file.Cases.Count);
        Assert.True(total >= 72, $"the corpus has only {total} cases");
        foreach (CorpusFile file in Files)
        {
            Assert.NotEmpty(file.Cases);
            Assert.NotNull(file.Meta);
            Assert.NotNull(file.Doc);
        }
    }

    /// <summary>Case ids are unique across the corpus, and every case says what it proves.</summary>
    [Fact]
    public void CaseIdsAreUniqueAndDescribed()
    {
        var ids = new HashSet<string>(StringComparer.Ordinal);
        foreach (CorpusFile file in Files)
        {
            foreach (CorpusCase testCase in file.Cases)
            {
                Assert.True(ids.Add(testCase.Id), $"{file.FileName}: duplicate case id {testCase.Id}");
                Assert.False(string.IsNullOrWhiteSpace(testCase.Description), $"{file.FileName} {testCase.Id}: needs a description");
                Assert.NotEmpty(testCase.Scripts);
                Assert.NotNull(testCase.State);
            }
        }
    }

    /// <summary>The corpus exercises every effect kind of v1 (the Rust runner asserts the same,
    /// and the two must not drift apart).</summary>
    [Fact]
    public void TheCorpusCoversEveryEffectKind()
    {
        var kinds = new HashSet<string>(StringComparer.Ordinal);
        foreach (CorpusFile file in Files)
        {
            foreach (CorpusCase testCase in file.Cases)
            {
                foreach (TomlTable effect in testCase.Expect.Tables("effects"))
                {
                    if (effect.String("kind") is { } kind)
                    {
                        kinds.Add(kind);
                    }

                    // A nested action effect is a kind the client must apply when chosen.
                    if (effect.Table("effect")?.String("kind") is { } inner)
                    {
                        kinds.Add(inner);
                    }
                }
            }
        }

        // `batch` never survives validation: the server flattens it. A script must still
        // exercise it, which is what the Rust runner checks too.
        bool usedBatch = Files
            .SelectMany(file => file.Cases)
            .SelectMany(testCase => testCase.Scripts)
            .Any(script => script.String("source")?.Contains(".batch(", StringComparison.Ordinal) == true);
        Assert.True(usedBatch, "no script exercises the batch effect");
        foreach (string kind in EffectsProtocol.AllKinds)
        {
            if (kind == "batch")
            {
                continue;
            }

            Assert.True(kinds.Contains(kind), $"no case expects a `{kind}` effect");
        }
    }

    /// <summary>One corpus case: the same bytes produce the same form state.</summary>
    [Theory]
    [MemberData(nameof(AllCases))]
    public void TheCaseProducesTheCheckedInState(string fileName, string caseId)
    {
        CorpusCase testCase = Index[string.Create(CultureInfo.InvariantCulture, $"{fileName}|{caseId}")];
        CorpusFixture fixture = CorpusFixture.Build(testCase.File, testCase);

        Assert.True(
            fixture.ReadProblems.Count == 0,
            $"{fileName} {caseId}: the client refused the server's own effects: {Describe(fixture.ReadProblems)}");

        ApplyResult result = fixture.ApplyRound();
        Assert.True(result.Applied, $"{fileName} {caseId}: the round was not applied");
        Assert.False(result.Superseded, $"{fileName} {caseId}: the first round cannot be superseded");
        Assert.True(
            result.Problems.Count == 0,
            $"{fileName} {caseId}: the client dropped effects the server validated: {Describe(result.Problems)}");

        object? expected = testCase.State!.Members;
        object? actual = StateSnapshot.Render(fixture.State);
        IReadOnlyList<string> differences = StateSnapshot.Differences(actual, expected);
        Assert.True(
            differences.Count == 0,
            string.Create(
                CultureInfo.InvariantCulture,
                $"{fileName} {caseId} ({testCase.Description}) differs:{Environment.NewLine}{string.Join(Environment.NewLine, differences)}"));
    }

    /// <summary>Two runs of the same case give the same state: the corpus path reads no clock
    /// and no random source.</summary>
    [Theory]
    [MemberData(nameof(AllCases))]
    public void TheCaseIsDeterministic(string fileName, string caseId)
    {
        CorpusCase testCase = Index[string.Create(CultureInfo.InvariantCulture, $"{fileName}|{caseId}")];
        CorpusFixture first = CorpusFixture.Build(testCase.File, testCase);
        CorpusFixture second = CorpusFixture.Build(testCase.File, testCase);
        first.ApplyRound();
        second.ApplyRound();
        Assert.True(
            StateSnapshot.Equal(StateSnapshot.Render(first.State), StateSnapshot.Render(second.State)),
            $"{fileName} {caseId}: two runs differ");
    }

    private static Dictionary<string, CorpusCase> BuildIndex()
    {
        var index = new Dictionary<string, CorpusCase>(StringComparer.Ordinal);
        foreach (CorpusFile file in Files)
        {
            foreach (CorpusCase testCase in file.Cases)
            {
                index.Add(string.Create(CultureInfo.InvariantCulture, $"{file.FileName}|{testCase.Id}"), testCase);
            }
        }

        return index;
    }

    private static string Describe(IReadOnlyList<EffectProblem> problems) =>
        string.Join("; ", problems.Select(problem => problem.ToString()));
}
