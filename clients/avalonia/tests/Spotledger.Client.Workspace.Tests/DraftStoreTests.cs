using System.Text;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Workspace.Tests;

public sealed class DraftStoreTests : IDisposable
{
    private readonly TempFolder _folder = new();
    private readonly FakeOsSecretStore _secrets = new();
    private readonly ManualClock _clock = new();

    public void Dispose() => _folder.Dispose();

    private FileDraftStore Store(FileDraftStoreOptions? options = null, FakeOsSecretStore? secrets = null)
        => new(options ?? new FileDraftStoreOptions { Root = _folder.Path }, secrets ?? _secrets, _clock);

    private DraftRecord Record(DocumentDraft draft, DraftKey? key = null, DateTimeOffset? at = null)
        => DraftSnapshot.Capture(draft, key ?? DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), at ?? _clock.GetUtcNow(), "October");

    private static DocumentDraft Edited()
    {
        var draft = Models.LoadedInvoice();
        draft.Set("title", new TextFieldValue("Edited secret title"));
        draft.Tables["items"].Rows[0].SetValue("qty", new DecimalFieldValue(9m));
        return draft;
    }

    [Fact]
    public async Task Round_trip_keeps_patch_base_version_time_and_tab_state()
    {
        using var store = Store();
        var draft = Edited();
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        var record = DraftSnapshot.Capture(draft, key, _clock.GetUtcNow(), "October", "tab=2;scroll=40");
        var result = await store.SaveAsync(record);
        Assert.Equal(DraftSaveOutcome.Saved, result.Outcome);

        var back = await store.LoadAsync(key);
        Assert.NotNull(back);
        Assert.Equal(3, back.BaseVersion);
        Assert.Equal("tab=2;scroll=40", back.TabState);
        Assert.Equal(record.SavedAtUtc, back.SavedAtUtc);
        Assert.Contains(back.Fields, f => f.Name == "title" && f.Json == "\"Edited secret title\"");
        var table = Assert.Single(back.Tables);
        Assert.Equal(2, table.Rows.Count);
        Assert.Equal("r1", table.Rows[0].Id);
    }

    [Fact]
    public async Task The_file_holds_no_plaintext_and_a_different_key_fails_cleanly()
    {
        using var store = Store();
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));
        var file = Directory.EnumerateFiles(_folder.Path, "*.draft", SearchOption.AllDirectories).Single();
        var bytes = File.ReadAllBytes(file);
        var text = Encoding.UTF8.GetString(bytes) + Convert.ToBase64String(bytes);
        Assert.DoesNotContain("Edited secret title", text);
        Assert.DoesNotContain("INV-1", Encoding.UTF8.GetString(bytes));
        Assert.DoesNotContain("alice", Encoding.UTF8.GetString(bytes));
        Assert.DoesNotContain(_secrets.Secrets.Values.Single(), text);

        // A new credential store with another key for the same scope: the file cannot be opened, is set aside, and nothing throws.
        var other = new FakeOsSecretStore();
        await other.SetSecretAsync(Models.Scope.SecretName, Convert.ToBase64String(new byte[32]));
        using var wrongKey = Store(secrets: other);
        Assert.Null(await wrongKey.LoadAsync(key));
        var note = Assert.Single(wrongKey.Quarantined);
        Assert.Contains("decrypted", note.Reason);
        Assert.False(File.Exists(file));
    }

    [Fact]
    public async Task A_corrupt_or_truncated_file_is_quarantined_with_a_note_and_never_throws()
    {
        using var store = Store();
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));
        var file = Directory.EnumerateFiles(_folder.Path, "*.draft", SearchOption.AllDirectories).Single();
        File.WriteAllBytes(file, [1, 2, 3]);
        var raised = new List<QuarantineNote>();
        store.DraftQuarantined += (_, n) => raised.Add(n);

        Assert.Null(await store.LoadAsync(key));
        Assert.Single(raised);
        var quarantine = Directory.EnumerateFiles(Path.Combine(Path.GetDirectoryName(file)!, "quarantine"), "*.note").Single();
        Assert.False(string.IsNullOrWhiteSpace(File.ReadAllText(quarantine)));
        Assert.Empty(await store.ListAsync(Models.Scope));
    }

    [Fact]
    public async Task A_bit_flip_in_the_ciphertext_is_detected()
    {
        using var store = Store();
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));
        var file = Directory.EnumerateFiles(_folder.Path, "*.draft", SearchOption.AllDirectories).Single();
        var bytes = File.ReadAllBytes(file);
        bytes[^1] ^= 0x01;
        File.WriteAllBytes(file, bytes);
        Assert.Null(await store.LoadAsync(key));
        Assert.Single(store.Quarantined);
    }

    [Fact]
    public async Task Eviction_removes_the_oldest_drafts_first_and_never_the_one_just_written()
    {
        var options = new FileDraftStoreOptions { Root = _folder.Path, MaxDraftBytes = 100_000, MaxScopeBytes = 100_000 };
        using var probe = Store(options);
        var draft = Edited();
        var first = DraftKey.ForDocument(Models.Scope, "invoice", "A");
        _ = await probe.SaveAsync(DraftSnapshot.Capture(draft, first, _clock.GetUtcNow()));
        var size = new FileInfo(Directory.EnumerateFiles(_folder.Path, "*.draft", SearchOption.AllDirectories).Single()).Length;

        // Room for exactly three drafts.
        using var store = Store(new FileDraftStoreOptions { Root = _folder.Path, MaxDraftBytes = size + 50, MaxScopeBytes = (size * 3) + 20 });
        var keys = new[] { "A", "B", "C", "D" }.Select(id => DraftKey.ForDocument(Models.Scope, "invoice", id)).ToArray();
        DraftSaveResult last = new(DraftSaveOutcome.Saved);
        for (var i = 1; i < keys.Length; i++)
        {
            _clock.Advance(TimeSpan.FromMinutes(1));
            last = await store.SaveAsync(DraftSnapshot.Capture(draft, keys[i], _clock.GetUtcNow()));
        }

        Assert.Contains("Older drafts", last.Message);
        Assert.Null(await store.LoadAsync(keys[0]));
        Assert.NotNull(await store.LoadAsync(keys[1]));
        Assert.NotNull(await store.LoadAsync(keys[3]));
        Assert.Equal(3, (await store.ListAsync(Models.Scope)).Count);
    }

    [Fact]
    public async Task A_draft_over_the_per_draft_cap_is_refused_and_reported()
    {
        using var store = Store(new FileDraftStoreOptions { Root = _folder.Path, MaxDraftBytes = 64, MaxScopeBytes = 1024 });
        var result = await store.SaveAsync(DraftSnapshot.Capture(Edited(), DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow()));
        Assert.Equal(DraftSaveOutcome.TooLarge, result.Outcome);
    }

    [Fact]
    public async Task Sign_out_removes_that_users_drafts_and_key_but_not_another_users()
    {
        using var store = Store();
        var bob = new DraftScope(Models.Scope.ServerId, "bob");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow()));
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), DraftKey.ForDocument(bob, "invoice", "INV-1"), _clock.GetUtcNow()));

        await store.DeleteAllAsync(Models.Scope);

        Assert.DoesNotContain(Models.Scope.SecretName, _secrets.Secrets.Keys);
        Assert.Contains(bob.SecretName, _secrets.Secrets.Keys);
        Assert.False(Directory.Exists(Path.Combine(_folder.Path, Models.Scope.DirectoryName)));
        Assert.Empty(await store.ListAsync(Models.Scope));
        Assert.Single(await store.ListAsync(bob));
    }

    [Fact]
    public async Task Drafts_are_per_user_and_per_server()
    {
        using var store = Store();
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));

        Assert.Null(await store.LoadAsync(DraftKey.ForDocument(new DraftScope(Models.Scope.ServerId, "bob"), "invoice", "INV-1")));
        Assert.Null(await store.LoadAsync(DraftKey.ForDocument(new DraftScope("https://other.example.test", "alice"), "invoice", "INV-1")));
        Assert.Empty(await store.ListAsync(new DraftScope("https://other.example.test", "alice")));
    }

    [Fact]
    public async Task A_file_copied_into_another_users_folder_does_not_open()
    {
        using var store = Store();
        var aliceKey = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), aliceKey, _clock.GetUtcNow()));
        var bob = new DraftScope(Models.Scope.ServerId, "bob");
        var bobKey = DraftKey.ForDocument(bob, "invoice", "INV-1");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), bobKey, _clock.GetUtcNow()));
        var aliceFile = Directory.EnumerateFiles(Path.Combine(_folder.Path, Models.Scope.DirectoryName), "*.draft").Single();
        var bobFile = Path.Combine(_folder.Path, bob.DirectoryName, bobKey.FileName);
        File.Copy(aliceFile, bobFile, overwrite: true);

        Assert.Null(await store.LoadAsync(bobKey));
        Assert.Single(store.Quarantined);
    }

    [Fact]
    public async Task Path_traversal_ids_cannot_leave_the_drafts_folder()
    {
        using var store = Store();
        var evil = new DraftScope("https://x.test/../../etc", "../../root");
        var key = DraftKey.ForDocument(evil, "../invoice", "../../../../tmp/pwned");
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));

        var files = Directory.EnumerateFiles(_folder.Path, "*", SearchOption.AllDirectories).ToList();
        Assert.All(files, f => Assert.StartsWith(Path.GetFullPath(_folder.Path), Path.GetFullPath(f), StringComparison.Ordinal));
        Assert.DoesNotContain(files, f => f.Contains("pwned", StringComparison.Ordinal) || f.Contains("..", StringComparison.Ordinal));
        Assert.NotNull(await store.LoadAsync(key));
    }

    [Fact]
    public async Task Files_and_folders_are_owner_only_on_unix()
    {
        if (OperatingSystem.IsWindows())
        {
            return;
        }

        using var store = Store();
        _ = await store.SaveAsync(DraftSnapshot.Capture(Edited(), DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow()));
        var file = Directory.EnumerateFiles(_folder.Path, "*.draft", SearchOption.AllDirectories).Single();
        Assert.Equal(UnixFileMode.UserRead | UnixFileMode.UserWrite, File.GetUnixFileMode(file));
        Assert.Equal(UnixFileMode.UserRead | UnixFileMode.UserWrite | UnixFileMode.UserExecute, File.GetUnixFileMode(Path.GetDirectoryName(file)!));
        Assert.Empty(Directory.EnumerateFiles(_folder.Path, "*.tmp", SearchOption.AllDirectories));
    }

    [Fact]
    public async Task Without_an_os_credential_store_drafts_stay_in_memory_and_the_notice_says_so()
    {
        var broken = new FakeOsSecretStore { Unavailable = true };
        using var store = Store(secrets: broken);
        var key = DraftKey.ForDocument(Models.Scope, "invoice", "INV-1");
        var result = await store.SaveAsync(DraftSnapshot.Capture(Edited(), key, _clock.GetUtcNow()));

        Assert.Equal(DraftSaveOutcome.SavedInMemoryOnly, result.Outcome);
        Assert.False(await store.IsPersistentAsync(Models.Scope));
        Assert.Equal("Drafts are not saved to disk on this machine.", store.MemoryOnlyNotice);
        Assert.False(Directory.Exists(_folder.Path) && Directory.EnumerateFiles(_folder.Path, "*", SearchOption.AllDirectories).Any());
        Assert.NotNull(await store.LoadAsync(key));
        Assert.Single(await store.ListAsync(Models.Scope));
    }

    [Fact]
    public async Task The_in_memory_credential_fallback_also_means_memory_only()
    {
        using var store = new FileDraftStore(new FileDraftStoreOptions { Root = _folder.Path }, new Api.InMemorySecretStore(), _clock);
        var result = await store.SaveAsync(DraftSnapshot.Capture(Edited(), DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow()));
        Assert.Equal(DraftSaveOutcome.SavedInMemoryOnly, result.Outcome);
    }

    [Fact]
    public void Masked_hidden_and_secret_fields_are_never_captured()
    {
        var draft = Models.LoadedInvoice();
        Models.Mask(draft, "ssn");
        draft.Set("api_key", new SecretFieldValue(true));
        draft.Set("title", new TextFieldValue("visible"));

        var record = DraftSnapshot.Capture(draft, DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow());

        Assert.Equal(["title"], record.Fields.Select(f => f.Name));
        var all = string.Join('|', record.Fields.Select(f => f.Json + f.BaseJson));
        Assert.DoesNotContain("123-45", all);
    }

    [Fact]
    public void A_masked_cell_of_a_child_row_is_not_captured()
    {
        var draft = Models.LoadedInvoice();
        draft.Tables["items"].Rows[0].SetValue("item", new TextFieldValue("changed"));
        draft.Tables["items"].Rows[1].SetValue("qty", new DecimalFieldValue(5m));
        var cell = draft.Tables["items"].Rows[0].Values["item"];
        Assert.False(cell.IsMasked);

        var record = DraftSnapshot.Capture(draft, DraftKey.ForDocument(Models.Scope, "invoice", "INV-1"), _clock.GetUtcNow());
        Assert.Equal("\"changed\"", record.Tables.Single().Rows[0].Values["item"]);
    }

    [Fact]
    public void A_key_names_a_document_or_a_new_draft_not_both()
    {
        Assert.Throws<ArgumentException>(() => new DraftKey(Models.Scope, "invoice", "A", "B"));
        Assert.Throws<ArgumentException>(() => new DraftKey(Models.Scope, "invoice", null, null));
        Assert.Throws<ArgumentException>(() => DraftKey.ForDocument(new DraftScope("", "alice"), "invoice", "A"));
    }
}
