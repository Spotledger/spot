# Spotledger Avalonia client

Install the .NET 10 SDK on Windows or Fedora 44.
Open `Spotledger.Client.sln` in Visual Studio or your preferred editor.
Build from the repository root with `dotnet build clients/avalonia/Spotledger.Client.sln -c Release`.
Run the desktop shell with `dotnet run --project clients/avalonia/src/Spotledger.Client.Desktop`.
Run the Core tests with `dotnet test clients/avalonia/Spotledger.Client.sln -c Release`.
The Core project has no UI dependency; the Desktop project only renders the Avalonia shell.
