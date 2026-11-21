using Spotledger.AdminCenter.Identity;
using Xunit;

namespace Spotledger.AdminCenter.Identity.Tests;

public sealed class EmergencyAndQuestionsTests
{
    private static readonly DateTimeOffset Now = new(2026, 10, 23, 12, 0, 0, TimeSpan.Zero);

    private static EmergencyAccessViewModel Emergency(FakeGateway gw, FakeShell? shell = null, string user = "me") =>
        new(gw, shell ?? new FakeShell(), user);

    [Fact]
    public async Task ARequestNeedsAReasonAndADurationWithinThePolicy()
    {
        var gw = new FakeGateway();
        var vm = Emergency(gw);
        await vm.RefreshAsync();
        Assert.False(await vm.RequestAsync(new EmergencyRequestDraft("role-dba", "db", 30)));
        Assert.False(await vm.RequestAsync(new EmergencyRequestDraft("role-dba", "Production database is down", 0)));
        Assert.False(await vm.RequestAsync(new EmergencyRequestDraft("role-dba", "Production database is down", 61)));
        Assert.DoesNotContain("RequestEmergency", gw.Calls);
        Assert.True(await vm.RequestAsync(new EmergencyRequestDraft("role-dba", "Production database is down", 30)));
        Assert.Single(vm.Mine);
        Assert.False(await vm.RequestAsync(new EmergencyRequestDraft("role-dba", "Production database is down", 30)));
    }

    [Fact]
    public async Task TheBannerShowsWhileASessionIsActiveAndCountsDownFromTheGivenTime()
    {
        var gw = new FakeGateway();
        gw.Mine.Add(new EmergencyGrantInfo("g1", "me", "role-dba", "Outage", EmergencyPhase.Active, Now.AddMinutes(25)));
        var vm = Emergency(gw);
        Assert.Null(vm.Banner(Now));
        await vm.RefreshAsync();
        Assert.Contains("25 minute(s) left", vm.Banner(Now), StringComparison.Ordinal);
        Assert.Contains("recorded", vm.Banner(Now), StringComparison.Ordinal);
        Assert.Contains("ended", vm.Banner(Now.AddMinutes(26)), StringComparison.Ordinal);
        Assert.True(await vm.EndAsync());
        Assert.Null(vm.Banner(Now));
    }

    [Fact]
    public async Task AnApproverCannotApproveTheirOwnRequestAndARefusalNeedsAReason()
    {
        var gw = new FakeGateway();
        gw.ToApprove.AddRange([
            new EmergencyGrantInfo("g1", "me", "role-dba", "Outage", EmergencyPhase.AwaitingApproval, null),
            new EmergencyGrantInfo("g2", "dave", "role-dba", "Outage", EmergencyPhase.AwaitingApproval, null),
        ]);
        var vm = Emergency(gw);
        await vm.RefreshAsync();
        Assert.False(await vm.DecideAsync("g1", true, null));
        Assert.False(await vm.DecideAsync("g2", false, " "));
        Assert.False(await vm.DecideAsync("nope", true, null));
        Assert.DoesNotContain("DecideEmergency", gw.Calls);
        Assert.True(await vm.DecideAsync("g2", true, null));
        Assert.Single(vm.ToApprove);
    }

    [Fact]
    public async Task AControllerReviewsSessionsButNotTheirOwn()
    {
        var gw = new FakeGateway();
        gw.Reviews.AddRange([new EmergencyReviewTask("r1", "g1", "me", 4, false), new EmergencyReviewTask("r2", "g2", "dave", 9, true)]);
        var vm = Emergency(gw);
        await vm.RefreshAsync();
        Assert.False(await vm.ConfirmReviewAsync("r1"));
        Assert.Contains("yourself", vm.Error, StringComparison.Ordinal);
        Assert.False(await vm.FlagReviewAsync("r2", ""));
        Assert.True(await vm.FlagReviewAsync("r2", "Exported the customer list"));
        Assert.Single(vm.Reviews);
        Assert.DoesNotContain("ConfirmReview", gw.Calls);
    }

    private static QuestionsViewModel Questions(FakeGateway gw, bool canExport = true) => new(gw, new FakeShell(), canExport);

    [Fact]
    public void ThereAreFourTabs()
    {
        Assert.Equal(4, QuestionsViewModel.Tabs.Count);
        Assert.Equal(4, Enum.GetValues<QuestionKind>().Length);
    }

    [Fact]
    public async Task EachTabAsksItsOwnQuestionWithItsOwnInputs()
    {
        var gw = new FakeGateway { Answer = new AnswerPage([new AnswerRow("invoice:read", "Ann can read invoices", ["assignment a1", "role AP clerk", "duty view-invoices", "source manual"])], null) };
        var vm = Questions(gw);

        Assert.False(await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo)));
        Assert.True(await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann")));
        Assert.Equal(QuestionKind.WhatCanUserDo, gw.LastQuestion?.Kind);

        vm.Tab = QuestionKind.WhoCanDo;
        Assert.Null(vm.Answer);
        Assert.False(await vm.AskAsync(new QuestionRequest(QuestionKind.WhoCanDo)));
        Assert.True(await vm.AskAsync(new QuestionRequest(QuestionKind.WhoCanDo, Permission: "invoice:read")));

        vm.Tab = QuestionKind.WhoCanSee;
        Assert.False(await vm.AskAsync(new QuestionRequest(QuestionKind.WhoCanSee, Doctype: "invoice")));
        Assert.True(await vm.AskAsync(new QuestionRequest(QuestionKind.WhoCanSee, Doctype: "invoice", DocumentId: "inv-1")));

        vm.Tab = QuestionKind.WhatChanged;
        Assert.False(await vm.AskAsync(new QuestionRequest(QuestionKind.WhatChanged, From: new DateOnly(2026, 10, 2), To: new DateOnly(2026, 10, 1))));
        Assert.True(await vm.AskAsync(new QuestionRequest(QuestionKind.WhatChanged, From: new DateOnly(2026, 10, 1), To: new DateOnly(2026, 10, 2))));
        Assert.Equal(QuestionKind.WhatChanged, gw.LastQuestion?.Kind);
    }

    [Fact]
    public async Task TheTabDecidesTheQuestionWhateverTheRequestSays()
    {
        var gw = new FakeGateway();
        var vm = Questions(gw);
        vm.Tab = QuestionKind.WhoCanDo;
        await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, Permission: "invoice:read"));
        Assert.Equal(QuestionKind.WhoCanDo, gw.LastQuestion?.Kind);
    }

    [Fact]
    public async Task TheWhyChainExpandsAndCollapses()
    {
        var row = new AnswerRow("invoice:read", "Ann can read invoices", ["assignment a1", "role AP clerk"]);
        var changeRow = new AnswerRow("2026-10-01", "Role granted", []);
        var gw = new FakeGateway { Answer = new AnswerPage([row, changeRow], null) };
        var vm = Questions(gw);
        await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann"));
        Assert.False(vm.IsExpanded(row));
        vm.ToggleWhy(row);
        Assert.True(vm.IsExpanded(row));
        vm.ToggleWhy(row);
        Assert.False(vm.IsExpanded(row));
        vm.ToggleWhy(changeRow);
        Assert.False(vm.IsExpanded(changeRow));
    }

    [Fact]
    public async Task ARefusedQuestionShowsTheServerMessageAndNoRows()
    {
        var gw = new FakeGateway();
        gw.Failures["Ask"] = new GatewayError("permission.denied", "Only access governance may ask this.");
        var vm = Questions(gw);
        Assert.False(await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann")));
        Assert.Null(vm.Answer);
        Assert.Equal("Only access governance may ask this.", vm.Error);
    }

    [Fact]
    public async Task PagingAppendsRows()
    {
        var gw = new FakeGateway { Answer = new AnswerPage([new AnswerRow("a", "A", [])], "next") };
        var vm = Questions(gw);
        await vm.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann"));
        gw.Answer = new AnswerPage([new AnswerRow("b", "B", [])], null);
        Assert.True(await vm.MoreAsync());
        Assert.Equal(2, vm.Answer?.Rows.Count);
        Assert.Equal("next", gw.LastQuestion?.Cursor);
        Assert.False(await vm.MoreAsync());
    }

    [Fact]
    public async Task ExportIsGatedByPermissionAndNeverReachesTheServerWithoutIt()
    {
        var gw = new FakeGateway();
        var denied = Questions(gw, canExport: false);
        await denied.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann"));
        Assert.False(denied.ExportAvailable);
        Assert.Null(await denied.ExportAsync());
        Assert.DoesNotContain("Export", gw.Calls);

        var allowed = Questions(gw);
        Assert.Null(await allowed.ExportAsync());
        await allowed.AskAsync(new QuestionRequest(QuestionKind.WhatCanUserDo, UserId: "ann"));
        Assert.True(allowed.ExportAvailable);
        Assert.StartsWith("subject", await allowed.ExportAsync(), StringComparison.Ordinal);
    }

    [Fact]
    public void EveryScreenRegistersAToolEntryWithAUniqueId()
    {
        var ids = IdentityTools.Entries.Select(e => e.Id).ToList();
        Assert.Equal(ids.Count, ids.Distinct().Count());
        Assert.Equal(8, ids.Count);
        Assert.All(IdentityTools.Entries, e => Assert.False(string.IsNullOrWhiteSpace(e.RequiredDuty)));
    }
}
