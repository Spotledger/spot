using Avalonia.Automation;
using Avalonia.Automation.Peers;
using Avalonia.Headless.XUnit;
using Spotledger.Client.Controls;
using Spotledger.Client.Metadata;
using Xunit;

namespace Spotledger.Client.Controls.Tests;

public sealed class SecretAndTextTests
{
    [AvaloniaFact]
    public void SecretOnlyCarriesSetStateAndReplaceCommand()
    {
        var input = new SpotSecretInput { Value = new SecretFieldValue(true) };
        var window = TestHost.Show(input);
        Assert.Equal("Set", input.StatusText);
        var replacements = 0;
        input.ReplaceRequested += (_, _) => replacements++;
        input.RequestReplace();
        Assert.Equal(1, replacements);
        input.IsReadOnly = true;
        input.RequestReplace();
        Assert.Equal(1, replacements);
        Assert.DoesNotContain(input.GetType().GetProperties(), p => p.Name is "Password" or "Secret" or "Text");
        Assert.DoesNotContain("password", input.ToString(), StringComparison.OrdinalIgnoreCase);
        Assert.Equal("Secret", AutomationProperties.GetName(input));
        Assert.NotNull(ControlAutomationPeer.CreatePeerForElement(input));
        input.Value = new SecretFieldValue(false);
        Assert.Equal("Not set", input.StatusText);
    }

    [AvaloniaFact]
    public void PreviewIsOnlyAvailableForMarkdownAndNeverExecutesHtml()
    {
        var input = new SpotTextArea { Text = "# Heading\n**strong**\n<script>alert(1)</script>", Format = "plain" };
        var window = TestHost.Show(input);
        input.IsPreviewOpen = true;
        Assert.False(input.IsPreviewOpen);
        input.Format = "markdown";
        input.IsPreviewOpen = true;
        Assert.True(input.IsPreviewOpen);
        Assert.Contains("Heading", input.PreviewText);
        Assert.DoesNotContain("**", input.PreviewText);
        input.Format = "code";
        Assert.False(input.IsPreviewOpen);
        Assert.True(input.IsMonospace);
        input.IsReadOnly = true;
        Assert.True(input.Editor.IsReadOnly);
    }
}
