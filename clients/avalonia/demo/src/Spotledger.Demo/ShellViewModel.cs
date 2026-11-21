using System.Globalization;
using Spotledger.Client.Controls;
using Spotledger.Demo.Mock;

namespace Spotledger.Demo;

public sealed class ShellViewModel : ObservableModel
{
    public IBackend Backend { get; }
    public HomeData Home { get; private set; } = null!;
    public CommandRegistry Registry { get; private set; } = null!;
    public CommandDispatcher Dispatcher { get; private set; } = null!;
    public TabManager Tabs { get; } = new();
    public DemoSettings Settings { get; private set; } = new(true, false, EnforcementLevel.Guided);
    public KeymapProfile Keymap { get; private set; } = KeymapProfile.Load("desktop");
    public SidebarMode Sidebar { get; private set; }
    public bool RailVisible { get; private set; } = true;
    public bool Urdu { get; private set; }
    public DensityMode DensityMode { get; private set; } = DensityMode.Comfortable;
    public string StatusMessage { get; private set; } = "";
    public Func<DemoTab, Task<DirtyChoice>> AskDirtyAsync { get; set; } = _ => Task.FromResult(DirtyChoice.Cancel);
    public event Action? LauncherRequested;
    public event Action? OverflowRequested;
    public event Action<string>? InformationRequested;
    public event Action? ExitRequested;
    private readonly Dictionary<Guid, (SidebarMode Sidebar, bool Rail)> layouts = [];
    public ShellViewModel(IBackend backend) => Backend = backend;
    public async Task InitializeAsync()
    {
        var response = await Backend.GetHomeAsync();
        Home = response.Value ?? throw new InvalidOperationException(response.Error!.Message);
        Registry = CommandRegistry.Create(Home);
        Keymap.Validate(Registry);
        Dispatcher = new(Backend, Registry);
        foreach (var definition in Registry.Definitions)
        {
            var id = definition.Id;
            Dispatcher.Register(id, context => HandleAsync(id, context));
        }
        OpenHome();
        Refresh();
    }
    public CommandContext Context => new(Home.Company.Id, Home.Company.Establishments[0].Id,
        Tabs.Active?.Record?.Id, Tabs.Active?.Key ?? "home");
    public async Task<DispatchResult> RunAsync(string id, CommandRoute route, CommandContext? context = null)
    {
        var result = await Dispatcher.ExecuteAsync(id, context ?? Context, route);
        if (!result.Executed && !result.Duplicate) StatusMessage = result.Availability.Reason;
        Refresh();
        return result;
    }
    private async Task HandleAsync(string id, CommandContext context)
    {
        switch (id)
        {
            case "shell.launcher": LauncherRequested?.Invoke(); return;
            case "shell.exit": if (await ResolveDirtyAsync(Tabs.Tabs)) ExitRequested?.Invoke(); return;
            case "tab.overflow": OverflowRequested?.Invoke(); return;
            case "document.save":
                var target = Tabs.Tabs.FirstOrDefault(t => t.Record?.Id == context.RecordId);
                if (target is not null) await SaveAsync(target);
                return;
            case "edit.note": OpenRecord(Home.Records[0]); return;
            case "tab.close": if (Tabs.Active is { } active) await CloseAsync([active]); return;
            case "tab.others": if (Tabs.Active is { } selected) await CloseAsync(Tabs.Others(selected)); return;
            case "tab.right": if (Tabs.Active is { } current) await CloseAsync(Tabs.RightOf(current)); return;
            case "tab.pin": if (Tabs.Active is { } pin) pin.IsPinned = !pin.IsPinned; return;
            case "tab.next": Tabs.Cycle(1); return;
            case "tab.previous": Tabs.Cycle(-1); return;
            case "workspace.home": OpenHome(); return;
            case "view.sidebar": Sidebar = (SidebarMode)(((int)Sidebar + 1) % 3); RememberLayout(); return;
            case "view.rail": RailVisible = !RailVisible; RememberLayout(); return;
            case "demo.reset": await ResetAsync(); return;
            case "demo.governance": await ConfigureAsync(Settings with { Governance = !Settings.Governance }); return;
            case "demo.maintenance": await ConfigureAsync(Settings with { Maintenance = !Settings.Maintenance }); return;
            case "help.keymap": InformationRequested?.Invoke(KeymapProfile.GenerateMarkdown()); return;
            case "help.guide": InformationRequested?.Invoke(L("GuideText")); return;
            case "help.about": InformationRequested?.Invoke(L("AboutText")); return;
            case "tools.admin": InformationRequested?.Invoke(L("AdminText")); return;
        }
        if (id.StartsWith("demo.role.", StringComparison.Ordinal))
            await SwitchRoleAsync(Enum.Parse<DemoRole>(id[10..]));
        else if (id.StartsWith("demo.enforcement.", StringComparison.Ordinal))
            await ConfigureAsync(Settings with { Enforcement = Enum.Parse<EnforcementLevel>(id[17..]) });
        else if (id.StartsWith("demo.keymap.", StringComparison.Ordinal)) { Keymap = KeymapProfile.Load(id[12..]); Keymap.Validate(Registry); }
        else if (id.StartsWith("demo.locale.", StringComparison.Ordinal)) Urdu = id.EndsWith("ur", StringComparison.Ordinal);
        else if (id.StartsWith("demo.density.", StringComparison.Ordinal))
            DensityMode = id.EndsWith("compact", StringComparison.Ordinal) ? DensityMode.Compact : DensityMode.Comfortable;
        else if (id.StartsWith("record.", StringComparison.Ordinal))
            OpenRecord(Home.Records.Single(r => r.Id == Guid.Parse(id[7..])));
        else OpenPlaceholder(id);
    }
    public void SelectTab(DemoTab tab) { Tabs.Activate(tab); Refresh(); }
    private void OpenHome() => Tabs.Open("home", "Home", Home.Company.Id);
    public void OpenRecord(DemoRecord record) => Tabs.Open(record.Id.ToString(), record.Title + " · " + record.Number, Home.Company.Id, record);
    private void OpenPlaceholder(string id) => Tabs.Open(id, Registry[id].Label, Home.Company.Id);
    public string Boundary(DemoTab tab)
    {
        var part = tab.Key.Contains("report", StringComparison.Ordinal) ? "5" :
            tab.Key.Contains("pos", StringComparison.Ordinal) ? "4" :
            tab.Key.Contains("sale", StringComparison.Ordinal) || tab.Key.Contains("sell", StringComparison.Ordinal) ? "3" : "2";
        return $"{L("Placeholder")} {part}. {L("NoPosting")}";
    }
    public async Task<bool> SaveAsync(DemoTab tab)
    {
        if (tab.Record is null) return true;
        var response = await Backend.PatchRecordAsync(tab.Record.Id, tab.Record.Version,
            new Dictionary<string, string?> { ["note"] = tab.Note });
        if (!response.IsSuccess) { StatusMessage = response.Error!.Message; Refresh(); return false; }
        tab.Accept(response.Value!);
        Home = (await Backend.GetHomeAsync()).Value!;
        return true;
    }
    private async Task<bool> ResolveDirtyAsync(IEnumerable<DemoTab> targets)
    {
        // Gather decisions before changing presentation state: Cancel leaves every tab intact.
        var choices = new List<(DemoTab Tab, DirtyChoice Choice)>();
        foreach (var tab in targets.Where(t => t.IsDirty).ToArray())
        {
            var choice = await AskDirtyAsync(tab);
            if (choice == DirtyChoice.Cancel) return false;
            choices.Add((tab, choice));
        }
        foreach (var (tab, choice) in choices)
            if (choice == DirtyChoice.Save && !await SaveAsync(tab)) return false;
        foreach (var (tab, choice) in choices)
            if (choice == DirtyChoice.Discard && tab.Record is { } record) tab.Accept(record);
        return true;
    }
    public async Task<bool> CloseAsync(IEnumerable<DemoTab> targets)
    {
        var selection = targets.ToArray();
        if (!await ResolveDirtyAsync(selection)) return false;
        Tabs.Remove(selection);
        if (Tabs.Tabs.Count == 0) OpenHome();
        Refresh();
        return true;
    }
    public async Task<bool> SwitchRoleAsync(DemoRole role)
    {
        if (!await ResolveDirtyAsync(Tabs.Tabs)) return false;
        var response = await Backend.SwitchRoleAsync(role);
        if (!response.IsSuccess) { StatusMessage = response.Error!.Message; return false; }
        Home = response.Value!;
        var layout = layouts.GetValueOrDefault(Home.User.Id, (SidebarMode.Expanded, true));
        Sidebar = layout.Item1; RailVisible = layout.Item2;
        OpenHome();
        Refresh();
        return true;
    }
    private void RememberLayout() => layouts[Home.User.Id] = (Sidebar, RailVisible);
    private async Task ConfigureAsync(DemoSettings settings)
    {
        var response = await Backend.ConfigureAsync(settings);
        if (response.IsSuccess) Settings = response.Value!;
        else StatusMessage = response.Error!.Message;
    }
    public async Task<bool> ResetAsync()
    {
        if (!await ResolveDirtyAsync(Tabs.Tabs)) return false;
        var response = await Backend.ResetAsync();
        if (!response.IsSuccess) { StatusMessage = response.Error!.Message; return false; }
        Home = response.Value!;
        Tabs.Remove(Tabs.Tabs.ToArray());
        Settings = new(true, false, EnforcementLevel.Guided);
        layouts.Clear(); Sidebar = SidebarMode.Expanded; RailVisible = true;
        Keymap = KeymapProfile.Load("desktop"); Urdu = false; DensityMode = DensityMode.Comfortable; StatusMessage = "";
        OpenHome(); Refresh(); return true;
    }
    public void Refresh() => Changed(nameof(Home));
    public string Number(decimal value) => value.ToString("N0", CultureInfo.GetCultureInfo(Urdu ? "ur-PK" : "en-GB"));
    public string Date(DateTimeOffset value) => value.ToString("g", CultureInfo.GetCultureInfo(Urdu ? "ur-PK" : "en-GB"));
    public string L(string key) => Urdu ? UrduLabels.GetValueOrDefault(key, key) : EnglishLabels.GetValueOrDefault(key, key);
    private static readonly Dictionary<string, string> EnglishLabels = new(StringComparer.Ordinal)
    {
        ["Greeting"] = "Welcome", ["TaskEmpty"] = "No assigned tasks. Open Reports to review process health.",
        ["Placeholder"] = "This placeholder is delivered in part", ["NoPosting"] = "Demo notes can be edited; no posting is simulated.",
        ["GuideText"] = "Part 1: Alt menus, Ctrl+K launcher, shared command availability, UUID tabs, editable seed notes, Save/Discard/Cancel, role-aware Home, simple mode, per-user sidebar/rail state, keymaps, banners, Urdu and density. Later parts deliver processes, forms, POS and reports. Use Demo → Reset to restore fixtures.",
        ["AboutText"] = "Spotledger · WP-UI-18 part 1. Mock backend, synthetic data, session-only layout. No real server, ledger or sync.",
        ["AdminText"] = "Admin Center is outside the demo (corrections section 9)."
    };
    private static readonly Dictionary<string, string> UrduLabels = new(StringComparer.Ordinal)
    {
        ["Home"] = "ہوم", ["My Tasks"] = "میرے کام", ["Processes"] = "کاروباری عمل", ["Sell"] = "فروخت", ["Buy"] = "خریداری",
        ["Inventory"] = "اسٹاک", ["Reports"] = "رپورٹس", ["Favorites"] = "پسندیدہ", ["File"] = "فائل", ["Edit"] = "ترمیم",
        ["View"] = "منظر", ["Transactions"] = "لین دین", ["Tools"] = "اوزار", ["Window"] = "ونڈو", ["Help"] = "مدد", ["Demo"] = "ڈیمو",
        ["Greeting"] = "خوش آمدید", ["Role shortcuts"] = "کردار کے کام", ["KPIs"] = "اہم اعداد", ["Process overview"] = "عمل کا جائزہ",
        ["Recent activity"] = "حالیہ سرگرمی", ["Context"] = "تفصیل", ["Command launcher"] = "کمانڈ تلاش", ["Search commands"] = "کمانڈ تلاش کریں",
        ["Maintenance mode"] = "دیکھ بھال کا موڈ", ["Enforcement"] = "نفاذ", ["Guided"] = "رہنمائی", ["Standard"] = "معیاری", ["Strict"] = "سخت",
        ["Mock connected"] = "ڈیمو منسلک", ["Sync simulated"] = "ہم وقت سازی فرضی", ["Role"] = "کردار", ["Company"] = "کمپنی", ["Establishment"] = "شاخ",
        ["Full Home"] = "مکمل ہوم", ["Simple Home"] = "سادہ ہوم", ["Source"] = "ماخذ", ["As of"] = "بوقت", ["Active"] = "فعال", ["Blocked"] = "رکا ہوا",
        ["Due today"] = "آج واجب", ["Overdue"] = "مدت گزر گئی", ["Upcoming"] = "آنے والا", ["Owner"] = "ذمہ دار", ["TaskEmpty"] = "کوئی کام نہیں۔ رپورٹس دیکھیں۔",
        ["Save"] = "محفوظ کریں", ["Discard"] = "رد کریں", ["Cancel"] = "منسوخ", ["Unsaved changes"] = "غیر محفوظ تبدیلیاں",
        ["Close"] = "بند کریں", ["All tabs"] = "تمام ٹیب", ["Pin"] = "پن", ["Unpin"] = "پن ہٹائیں", ["Close others"] = "دیگر بند کریں",
        ["Close to the right"] = "دائیں ٹیب بند کریں", ["Resolve"] = "حل کریں", ["Placeholder"] = "یہ اسکرین بعد کے حصے میں آئے گی",
        ["NoPosting"] = "صرف ڈیمو نوٹ میں ترمیم ممکن ہے۔", ["Note"] = "نوٹ", ["Filter"] = "فلٹر", ["Draft"] = "مسودہ", ["Post document"] = "دستاویز پوسٹ کریں",
        ["Purchase Request"] = "خریداری کی درخواست", ["Receive Goods"] = "سامان وصول کریں", ["Supplier Invoice"] = "سپلائر بل",
        ["Invoice Customer"] = "گاہک کا بل", ["Approvals"] = "منظوری", ["Checkout"] = "کیش کاؤنٹر", ["Purchases this month"] = "اس ماہ خریداری",
        ["Stock value"] = "اسٹاک کی قدر", ["Outstanding receivables"] = "بقایا وصولیاں", ["Procure to Pay"] = "خریداری سے ادائیگی",
        ["Review supporting evidence"] = "ثبوت دیکھیں", ["Source purchase request"] = "خریداری کی درخواست دیکھیں",
        ["Approve purchase order"] = "خریداری آرڈر منظور کریں", ["Receive expected goods"] = "متوقع سامان وصول کریں",
        ["Review supplier invoice"] = "سپلائر بل دیکھیں", ["Prepare customer invoice"] = "گاہک کا بل تیار کریں", ["Open checkout shift"] = "کیش شفٹ کھولیں",
        ["Create"] = "بنائیں", ["Open"] = "کھولیں", ["Recent"] = "حالیہ", ["Compact"] = "مختصر", ["Comfortable"] = "آرام دہ",
        ["Reset demo"] = "ڈیمو دوبارہ شروع", ["Keymap"] = "کی بورڈ نقشہ", ["Locale"] = "زبان", ["Density"] = "کثافت", ["Procurement"] = "خریداری", ["Sales"] = "فروخت",
        ["Toggle sidebar"] = "کنارے کا مینو", ["Toggle right rail"] = "تفصیل دکھائیں", ["Keyboard map"] = "کی بورڈ نقشہ", ["Demo guide"] = "ڈیمو رہنما",
        ["Toggle governance / simple Home"] = "سادہ یا مکمل ہوم", ["Toggle maintenance"] = "دیکھ بھال", ["English"] = "انگریزی", ["Urdu"] = "اردو",
        ["Procurement officer"] = "خریداری افسر", ["Approver"] = "منظوری افسر", ["Warehouse operator"] = "گودام افسر", ["Accounts payable clerk"] = "ادائیگی افسر",
        ["Sales clerk"] = "فروخت افسر", ["Cashier"] = "کیشیئر", ["Manager"] = "مینیجر", ["All records"] = "تمام ریکارڈ", ["Head office"] = "مرکزی دفتر", ["Harbour store"] = "بندرگاہ شاخ",
        ["GuideText"] = "یہ پہلا حصہ ہے: مینو، تلاش، ٹیب، کردار، نوٹ، کی بورڈ، اردو اور کثافت۔ ڈیمو مینو سے دوبارہ شروع کریں۔",
        ["AboutText"] = "اسپاٹ لیجر ڈیمو، پہلا حصہ۔ فرضی ڈیٹا اور عارضی ترتیبات۔", ["AdminText"] = "انتظامی مرکز ڈیمو سے باہر ہے۔"
    };
}
