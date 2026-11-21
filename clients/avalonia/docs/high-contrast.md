# High contrast in the Avalonia client

WP-UI-15.T3 adds the two high-contrast palettes the accessibility review asks for, the tokens a
control needs to draw a visible focus ring, a detector the platform answers through, and the
tests that hold the contrast floors.

## 1. The four variants

`clients/avalonia/design/tokens.json` now declares four colour variants. The generator writes a
theme dictionary for each, in a stable order:

| variant | dictionary key | painted on | used when |
| --- | --- | --- | --- |
| `light` | `Light` | `#FFFFFF` canvas | the default |
| `dark` | `Dark` | `#0B1220` canvas | the user picks dark |
| `highContrast` | `{x:Static theme:ThemeVariants.HighContrast}` | `#FFFFFF` canvas, black text | platform high contrast, light palette |
| `highContrastDark` | `{x:Static theme:ThemeVariants.HighContrastDark}` | `#000000` canvas, white text | platform high contrast, dark palette |

Avalonia's XAML type converter only accepts built-in variants given as text, so the two custom
keys are emitted as `x:Static` references to the `ThemeVariant` fields on `ThemeVariants`. Each
custom variant inherits from `Light`/`Dark`, so any control theme that only knows the built-in
pair still resolves while our tokens follow the high-contrast dictionary.

`Tokens.cs` gains the focus role keys the same way (`Color.Focus.Ring`, `Focus.Width`,
`Focus.Offset`).

## 2. What the contrast tests hold

`TokenContractTests` reads `tokens.json` and computes WCAG relative luminance itself; nothing is
asserted from a hand-written table.

- **Every ordinary text pair** (`text.primary`, `text.secondary` on `surface.canvas` and
  `surface.raised`) is at least **4.5:1** in `light` and `dark`.
- **Every high-contrast text pair** — primary, secondary, disabled and all four semantic colours
  on all three surfaces — is at least **7:1** in both high-contrast variants.
- **The focus ring** is at least **3:1** against `surface.canvas`, `surface.raised` and
  `surface.sunken` in all four variants.
- **High-contrast borders** (`border.default`, `border.strong`) are at least **3:1** against the
  canvas and raised surfaces.
- **The ring is a real ring**: `Focus.Width` is at least **3 px** and `Focus.Offset` is a
  non-negative dimension.

Measured values, for review:

| variant | text.primary | text.secondary | text.disabled | min text/surface | ring/canvas | ring/sunken | border/canvas |
|---|---|---|---|---|---|---|---|
| `light` | 17.74 | 10.35 | 4.76 | 4.34 | 5.17 | 4.72 | 1.48 |
| `dark` | 17.89 | 12.61 | 7.30 | 5.71 | 7.36 | 5.75 | 2.47 |
| `highContrast` | 21.00 | 16.48 | 10.37 | 9.26 | 8.95 | 8.00 | 7.00 |
| `highContrastDark` | 21.00 | 16.83 | 10.02 | 8.30 | 10.47 | 8.68 | 6.25 |

The `light` minimum of 4.34 is `text.disabled` on the sunken surface. Inactive text is exempt
from the AA minimum in WCAG 2.2 (1.4.3) and the ordinary palettes are only required to reach
4.5:1 for primary and secondary; the high-contrast palettes hold 7:1 for disabled text too,
because there the reader has explicitly asked for maximum legibility.

The generator itself is covered: every colour token must exist in every declared variant
(`MissingVariantValueIsRejectedByTheGenerator`), the generated files must match the source
(`GeneratedAvaloniaResourcesAndTypedKeysMatchTheSource`), and a hand edit of a token value is
detected as stale (`AHandEditedTokenFileIsDetectedAsStale`, plus the build-time
`CheckGeneratedThemeTokens` target).

## 3. Applying a variant

```csharp
SpotledgerTheme.Apply(application, ThemeVariantKind.HighContrastDark);
SpotledgerTheme.ActiveVariant(application);                       // ThemeVariantKind.HighContrastDark
SpotledgerTheme.TryGetToken(application, Tokens.TokenColorTextPrimary, variant, out var value);
```

`Apply` merges the token dictionary (idempotently) and sets `Application.RequestedThemeVariant`.
`ApplyingEachVariantResolvesThatVariantsOwnValues` applies all four and checks the resolved
`text.primary` and `surface.canvas` against `tokens.json`, so a mis-keyed dictionary cannot pass.

## 4. The detector

```csharp
public interface IHighContrastDetector { HighContrastState Read(); }
```

`HighContrastState` is `(bool Enabled, bool Dark)`. The platform question is a pure read:

- **Windows**: `WindowsHighContrastDetector` calls
  `SystemParametersInfo(SPI_GETHIGHCONTRAST, …)` and decodes the `HCF_HIGHCONTRASTON` bit with
  `HighContrastDetector.FromHighContrastFlags`. The P/Invoke lives only in
  `WindowsHighContrastDetector.cs`, is marked `[SupportedOSPlatform("windows")]`, and is reachable
  only behind the `OperatingSystem.IsWindows()` check in `HighContrastDetector.Create()`, so no
  other platform can load `user32.dll`. A failed call reads as "off" (fail open: a theme read must
  never take the window down).
- **Everywhere else**: `NoHighContrastDetector.Instance`, which always answers off.

`HighContrastDetector.Resolve(requested, state)` is the decision: high contrast overrides the
user's Light/Dark choice, and the user's choice stands when the platform reports high contrast
off. `SpotledgerTheme.ApplyForContrast(application, detector, requested)` applies it.

The Windows detector reports `Dark: false` and never infers a palette from the flag alone: the
Win32 API exposes one "high contrast on" bit, not which palette the desktop is using. Choosing
the dark palette stays a user or host decision until a follow-up reads the system colours.

## 5. What is not wired here

Wiring the detector into the running app is a follow-up, because `Spotledger.Client.Desktop` is
not an owned path of this task:

1. On start-up, call
   `SpotledgerTheme.ApplyForContrast(application, HighContrastDetector.Create(), <the user's Light/Dark setting>)`.
2. Re-read on the platform's accessibility-change notification (Win32 `WM_SETTINGCHANGE` with
   `SPI_SETHIGHCONTRAST`) and re-apply.
3. Tell the user in settings that the OS setting is winning, so a manual Light/Dark choice that
   does not take effect is explained.

The Browser host has no platform switch; `NoHighContrastDetector` is correct there, and a
`prefers-contrast` media query would be the browser equivalent in a follow-up.
