using System;
using System.Collections.Generic;
using System.Linq;
using Spotledger.Client.FlowDesigner;
using Xunit;

namespace Spotledger.Client.FlowDesigner.Run.Tests;

/// The redaction rules the client applies itself, with the server's marker syntax: a sensitive key's
/// value is the bare marker, a known value is a labelled marker, a value too short to match is
/// withheld, and the pattern rules keep the key or the scheme where that reads better.
public sealed class RedactionTests
{

    [Fact]

    public void TheMarkerSyntaxIsTheServers()
    {
        Assert.Equal("[redacted]", RunRedactor.Marker);
        Assert.Equal("[redacted:api]", RunRedactor.MarkerFor("api"));
        Assert.Equal("[redacted:", RunRedactor.MarkerPrefix);
    }

    [Fact]

    public void ASensitiveKeyNeverRendersItsValue()
    {
        RunRedactor redactor = new();
        string payload =
            """
            {"order":{"password":"hunter2-and-more","card_number":"4111111111111111","note":"ok"},"token":"abc"}
            """;

        string clean = redactor.RedactJson(payload);

        Assert.DoesNotContain("hunter2", clean, StringComparison.Ordinal);
        Assert.DoesNotContain("4111111111111111", clean, StringComparison.Ordinal);
        Assert.DoesNotContain("\"abc\"", clean, StringComparison.Ordinal);
        Assert.Contains("[redacted]", clean, StringComparison.Ordinal);
        Assert.Contains("\"note\": \"ok\"", clean, StringComparison.Ordinal);
    }

    [Fact]

    public void AValueOfASensitiveKeyIsNeverDescendedInto()
    {
        RunRedactor redactor = new();
        JsonTreeNode root = JsonTree.Parse("""{"credentials":{"user":"ana","password":"hunter2"}}""").Root!;

        JsonTreeNode clean = redactor.RedactTree(root);
        JsonTreeNode? credentials = clean.Child("credentials");

        Assert.NotNull(credentials);
        JsonTreeNode? password = credentials!.Child("password");
        Assert.NotNull(password);
        Assert.Equal(RunRedactor.Marker, password!.Text);
        Assert.True(password.IsRedacted);
    }

    [Fact]

    public void AKnownValueIsScrubbedFromEveryStringAndItsEncodedForms()
    {
        RunSecret secret = new("api", RunFixtures.Canary);
        RunRedactor redactor = new([secret]);
        string base64 = Convert.ToBase64String(System.Text.Encoding.UTF8.GetBytes(RunFixtures.Canary));
        string encoded = Uri.EscapeDataString(RunFixtures.Canary);

        string text = "a " + RunFixtures.Canary + " b " + base64 + " c " + encoded + " d";

        string clean = redactor.Redact(text);

        Assert.DoesNotContain(RunFixtures.Canary, clean, StringComparison.Ordinal);
        Assert.DoesNotContain(base64, clean, StringComparison.Ordinal);
        Assert.DoesNotContain(encoded, clean, StringComparison.Ordinal);
        Assert.Contains("[redacted:api]", clean, StringComparison.Ordinal);
        Assert.True(redactor.IsScrubbed(clean));
        Assert.False(redactor.IsScrubbed(text));
    }

    [Fact]

    public void AShortValueIsWithheldInsteadOfPrinted()
    {
        RunSecret shortSecret = new("pin", "hunter2");
        RunRedactor redactor = new([shortSecret]);

        Assert.False(RunRedactor.CanMatch("hunter2"));
        Assert.Equal(RunRedactor.Marker, redactor.Redact("the pin is hunter2"));
        Assert.Equal(RunRedactor.Marker, redactor.RedactOrPlaceholder("the pin is hunter2", shortSecret));
        Assert.Equal("nothing to see", redactor.Redact("nothing to see"));
    }

    [Fact]

    public void RedactionIsIdempotent()
    {
        RunRedactor redactor = new([new RunSecret("api", RunFixtures.Canary)]);
        string text = "Authorization: Bearer " + RunFixtures.Canary + " password=hunter2-and-more";

        string once = redactor.Redact(text);
        string twice = redactor.Redact(once);

        Assert.Equal(once, twice, StringComparer.Ordinal);
    }

    [Theory]
    [InlineData("Authorization: Bearer abcdefghijklmnop", "Authorization: [redacted:header]")]
    [InlineData("Authorization: Basic dXNlcjpwYXNzd29yZA==", "Authorization: [redacted:header]")]
    [InlineData("password=hunter2-and-more&x=1", "password=[redacted:parameter]&x=1")]
    [InlineData("https://svc:s3cr3t-value@api.test/x", "https://[redacted:credentials]@api.test/x")]
    [InlineData("sent with Bearer abcdefghijklmnop", "sent with Bearer [redacted:token]")]

    public void PatternsKeepTheKeyAndReplaceTheCredential(string text, string expected)
    {
        RunRedactor redactor = new();

        Assert.Equal(expected, redactor.Redact(text), StringComparer.Ordinal);
    }

    [Fact]

    public void ABearerTokenIsScrubbedEvenWhenNoValueIsRegistered()
    {
        RunRedactor redactor = new();

        string clean = redactor.Redact("Bearer sk-live-unregistered-token-value");

        Assert.DoesNotContain("sk-live-unregistered-token-value", clean, StringComparison.Ordinal);
        Assert.Contains("[redacted:token]", clean, StringComparison.Ordinal);
    }

    [Fact]

    public void AnUnparseablePayloadIsWithheld()
    {
        RunRedactor redactor = new();

        Assert.Equal(RunRedactor.Marker, redactor.RedactJson("{not json"));
        Assert.Null(redactor.RedactParsed("{not json"));
        Assert.Equal(string.Empty, redactor.RedactJson(null));
    }

    [Fact]

    public void TheSensitiveKeyListIsTheServers()
    {
        IReadOnlyList<string> keys = RunRedactor.SensitiveKeys;

        foreach (string key in new[] { "password", "secret", "token", "api_key", "authorization", "card_number" })
        {
            Assert.True(RunRedactor.IsSensitiveKey(key));
            Assert.Contains(key, keys);
        }

        Assert.True(RunRedactor.IsSensitiveKey("PASSWORD"));
        Assert.False(RunRedactor.IsSensitiveKey("note"));
        Assert.False(RunRedactor.IsSensitiveKey(null));
    }

    [Fact]

    public void ALabelFollowsTheServersGrammar()
    {
        Assert.True(RunSecret.IsLabel("sales_api"));
        Assert.False(RunSecret.IsLabel("Sales.Api"));
        Assert.False(RunSecret.IsLabel(string.Empty));
        Assert.False(RunSecret.IsLabel(new string('a', 21)));
        Assert.Equal("sales_api", RunSecret.DeriveLabel("Sales.Api"));
        Assert.Equal("___", RunSecret.DeriveLabel("..."));
        Assert.Throws<ArgumentException>(() => new RunSecret("Bad Label", "value"));
    }
}

/// The canary sweep of this task: a credential value the server sends by mistake never reaches a
/// rendered string. Every surface the history can paint is checked, and the markers that replace the
/// value stay visible, because a redaction a reader cannot see is a redaction a reader cannot trust.
public sealed class CanaryTests
{

    [Fact]

    public void AnUnregisteredCanaryNeverRendersAnywhereInTheHistory()
    {
        RunDetail detail = DetailWithCanary();
        RunStrings strings = new();
        RunHistory history = new(strings, new RunRedactor(), RunFixtures.Catalogue());

        history.SetRuns([detail.Summary]);
        history.Select(detail);

        AssertClean(history, RunFixtures.Canary);
        Assert.Contains("[redacted", history.ToText(), StringComparison.Ordinal);
    }

    [Fact]

    public void ARegisteredCanaryIsReplacedByItsLabel()
    {
        RunDetail detail = DetailWithCanary();
        RunRedactor redactor = new([new RunSecret("api", RunFixtures.Canary)]);
        RunHistory history = new(new RunStrings(), redactor, RunFixtures.Catalogue());

        history.Select(detail);

        AssertClean(history, RunFixtures.Canary);
        Assert.Contains("[redacted", history.ToText(), StringComparison.Ordinal);
    }

    [Fact]

    public async System.Threading.Tasks.Task ARefusalThatCarriesTheCanaryIsScrubbedBeforeItIsShown()
    {
        FakeRunServer server = new()
        {
            StartFault = new RunRefusedException("bpm.run.refused", "the peer echoed " + RunFixtures.Canary),
        };
        FlowRunSession session = new(
            server,
            RunFixtures.ScenarioDocument(),
            RunFixtures.Catalogue(),
            secrets: [new RunSecret("api", RunFixtures.Canary)]);
        session.SampleData.Text = RunFixtures.SampleJson;

        FlowActionResult result = await session.StartTestAsync(TestContext.Current.CancellationToken);

        Assert.False(result.Ok);
        Assert.NotNull(session.ErrorMessage);
        Assert.DoesNotContain(RunFixtures.Canary, session.ErrorMessage!, StringComparison.Ordinal);
        Assert.Contains("[redacted", session.ErrorMessage!, StringComparison.Ordinal);
    }

    [Fact]

    public void TheCanaryIsAbsentFromEveryPaneLineOfEveryAttempt()
    {
        RunHistory history = new(new RunStrings(), new RunRedactor(), RunFixtures.Catalogue());
        history.Select(DetailWithCanary());

        foreach (RunExchange exchange in history.Exchanges)
        {
            Assert.DoesNotContain(RunFixtures.Canary, exchange.InputText, StringComparison.Ordinal);
            Assert.DoesNotContain(RunFixtures.Canary, exchange.OutputText, StringComparison.Ordinal);
            foreach (string line in exchange.InputLines.Concat(exchange.OutputLines))
            {
                Assert.DoesNotContain(RunFixtures.Canary, line, StringComparison.Ordinal);
            }
        }
    }

    private static void AssertClean(RunHistory history, string canary)
    {
        Assert.DoesNotContain(canary, history.ToText(), StringComparison.Ordinal);
        foreach (RunTimelineRow row in history.Timeline)
        {
            Assert.DoesNotContain(canary, row.Text, StringComparison.Ordinal);
            Assert.DoesNotContain(canary, row.ErrorMessage ?? string.Empty, StringComparison.Ordinal);
        }
    }

    /// A run in which the server sends the raw canary in a token key, in prose, in an authorization
    /// header and in a failure reason: every route a value can take into a pane.
    private static RunDetail DetailWithCanary()
    {
        string canary = RunFixtures.Canary;
        RunSummary summary = new("run_9", "order_intake", 3, RunOutcome.Failed, RunFixtures.T0, RunFixtures.T0.AddSeconds(1), "ana", 1);
        List<RunNodeAttempt> attempts =
        [
            new("webhook", "webhook", 1, RunNodeStatus.Succeeded, RunFixtures.T0, RunFixtures.T0.AddMilliseconds(100),
                InputJson: "{\"token\":\"" + canary + "\"}",
                OutputJson: "{\"note\":\"the peer echoed Bearer " + canary + "\"}"),
            new("http", "http", 1, RunNodeStatus.Failed, RunFixtures.T0.AddMilliseconds(100), RunFixtures.T0.AddMilliseconds(900),
                ErrorCode: "bpm_integration.http_status",
                ErrorMessage: "HTTP 500 for Bearer " + canary,
                ErrorBranch: true,
                InputJson: "{\"url\":\"https://api.test/x\",\"authorization\":\"Bearer " + canary + "\"}"),
        ];
        return new RunDetail(summary, attempts);
    }
}
