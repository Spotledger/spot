using Avalonia.Headless.XUnit;
using Avalonia.Input;
using Spotledger.Client.Controls;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class MultiSelectTests
{
    [AvaloniaTheory]
    [InlineData(Key.Back, "", 1)]
    [InlineData(Key.Back, "query", 2)]
    [InlineData(Key.Delete, "", 2)]
    public void KeyboardRemovalOnlyUsesEmptyQueryBackspace(Key key, string query, int remaining)
    {
        var input = new SpotMultiSelect { SelectedItems = [new("one", "One"), new("two", "Two")] };
        TestHost.Show(input);
        input.Lookup.SearchText = query;
        input.HandleKey(key);
        Assert.Equal(remaining, input.SelectedItems.Count);
        input.IsReadOnly = true;
        input.HandleKey(Key.Back);
        Assert.Equal(remaining, input.SelectedItems.Count);
    }

    [AvaloniaFact]
    public async Task LookupKeyboardAddsUniqueChipsAndRemovePreservesOtherIds()
    {
        var input = new SpotMultiSelect { Source = new StaticLookupSource() };
        input.Lookup.Delay = new ImmediateLookupDelay();
        TestHost.Show(input);
        input.Lookup.SearchText = "one";
        await input.Lookup.PendingSearch;
        input.Lookup.HandleKey(Key.Enter);
        Assert.Equal("one", Assert.Single(input.SelectedItems).Id);
        input.Add(new("one", "Duplicate"));
        Assert.Single(input.SelectedItems);
        input.Add(new("two", "Two"));
        input.Remove("one");
        Assert.Equal("two", Assert.Single(input.SelectedItems).Id);
    }
}
