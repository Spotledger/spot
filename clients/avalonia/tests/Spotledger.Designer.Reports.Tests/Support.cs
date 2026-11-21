global using AsyncRelayCommand = Spotledger.Client.Designer.AsyncRelayCommand;

using Spotledger.Designer.Reports;
using Xunit;

namespace Spotledger.Designer.Reports.Tests;

internal static class Support
{
    public static async Task RunAsync(AsyncRelayCommand command)
    {
        Assert.True(command.CanExecute(null), "the command should be enabled");
        command.Execute(null);
        await command.ExecutionTask;
    }

    public static FieldChoice Choice(ReportBuilderViewModel builder, string path) =>
        builder.FieldChoices.Single(c => c.Path == path);
}
