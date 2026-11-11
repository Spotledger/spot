# Targets check: desktop and browser

The contract requires the model and the renderer to compile for the desktop head and the
browser head. Today the Avalonia solution has exactly one TFM (`net10.0`) and no browser
head project, so there is nothing to build a browser head *against* yet; adding a
`net10.0-browser` TFM here would pull `Avalonia.Browser` in, which this task may not add.
The gap is listed in the task's open questions for the head that lands it.

What is checked in CI now (all inside this test project):

1. **The model is framework-free.** `ContractParityTests.TheModelAssemblyReferencesNoUiFrameworkSoABrowserHeadCanReuseIt`
   reads the model assembly's referenced assemblies and fails if anything `Avalonia*` or
   `System.Drawing*` is there. The model is `net10.0`, `IsAotCompatible`, and uses only
   `System.Text.Json` readers (no reflection serialisation), so a browser head can reference
   it unchanged.
2. **The renderer uses no platform API.** It draws with `Avalonia.Controls` and the
   catalog/theme projects only: no P/Invoke, no registry, no file system, no
   `System.Drawing`, no `Assembly.Load`. The same build (`dotnet build Spotledger.Client.sln
   -warnaserror`) compiles it for the only TFM the solution has.
3. **The renderer has no hard-coded visuals.** `ContractParityTests.TheRendererHasNoHardCodedColourSizeOrFont`
   scans the renderer's sources for hex colours, literal brushes and numeric font sizes.

When a browser head lands (WP-UI-16): add a `net10.0-browser` TFM to
`Spotledger.Client.Extensions` and `Spotledger.Client.Extensions.Renderer`, then run

```
dotnet build clients/avalonia/src/Spotledger.Client.Extensions/Renderer/Spotledger.Client.Extensions.Renderer.csproj -f net10.0-browser -warnaserror
```

in CI next to the desktop build. Until then the two tests above are the guard: they fail
before a browser head would, with the same class of mistake (a UI-framework reference in the
model, or a platform-specific call in the renderer).

Tests themselves run headless on `net10.0` with Avalonia.Headless.XUnit and Skia
(`TestApp.cs`), so the screenshot and layout assertions are real rasterisations.
