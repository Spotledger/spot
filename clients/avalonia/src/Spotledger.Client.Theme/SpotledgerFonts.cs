using Avalonia.Media;
using Avalonia.Media.Fonts;
using Avalonia.Platform;

namespace Spotledger.Client.Theme;

/// <summary>
/// The bundled text fonts. Urdu is shipped with the client, not taken from the host: a
/// machine without a Naskh family used to render every Urdu label as boxes. The two
/// Noto Naskh Arabic faces are SIL OFL 1.1 files copied from the SPK-03 print spike
/// (<c>lab/SPK-03/assets</c>) and are compiled into this assembly as Avalonia resources
/// (see <c>Fonts/</c>), so <c>avares://</c> addressing works with no system font source.
///
/// Two mechanisms are set up, deliberately: an <see cref="EmbeddedFontCollection"/> keyed
/// on the font folder (so <c>Noto Naskh Arabic</c> resolves by *name* — what
/// <c>Font.Family.Urdu</c> lists first), and the URI form
/// <c>avares://Spotledger.Client.Theme/Fonts#Noto Naskh Arabic</c> (so a consumer can pin
/// the bundled face even if a system family of the same name exists).
/// </summary>
public static class SpotledgerFonts
{
    /// <summary>The family name inside the bundled faces (name id 1/16).</summary>
    public const string UrduFamilyName = "Noto Naskh Arabic";

    /// <summary>The folder that carries the bundled faces, as an Avalonia asset URI.</summary>
    public static Uri UrduSource { get; } = new("avares://Spotledger.Client.Theme/Fonts/");

    /// <summary>
    /// The collection key. Avalonia requires a custom font collection key to use the
    /// <c>fonts:</c> scheme, and a <see cref="FontFamily"/> addresses the collection by it
    /// (<c>fonts:SpotledgerNaskh#Noto Naskh Arabic</c>).
    /// </summary>
    public static Uri UrduCollectionKey { get; } = new("fonts:SpotledgerNaskh");

    /// <summary>
    /// The Urdu family: the bundled collection and nothing else. No trailing by-name fallbacks,
    /// on purpose. A host that ships its own "Noto Naskh Arabic" (the Ubuntu CI image does) would
    /// otherwise answer for any weight the bundle lacks — the field label asks for Medium, the
    /// bundle has Regular and Bold — and the same screen would render with a different version of
    /// the face on different machines. With one collection named, a missing weight is simulated
    /// from the bundled faces, so the glyphs are ours everywhere. Per-character fallback for
    /// characters the face lacks (Latin codes, punctuation) is unaffected: that goes through
    /// <c>FontManager.TryMatchCharacter</c>, not through this list.
    /// </summary>
    public static FontFamily Urdu { get; } = new($"{UrduCollectionKey}#{UrduFamilyName}");

    /// <summary>The regular (body) face of the bundled family.</summary>
    public static Uri UrduRegularAsset { get; } = new(UrduSource, "NotoNaskhArabic-Regular.ttf");

    /// <summary>The bold face of the bundled family.</summary>
    public static Uri UrduBoldAsset { get; } = new(UrduSource, "NotoNaskhArabic-Bold.ttf");

    /// <summary>The SIL OFL 1.1 text that ships beside the faces.</summary>
    public static Uri UrduLicenceAsset { get; } = new(UrduSource, "NotoNaskhArabic-OFL.txt");

    /// <summary>
    /// The Latin family name inside the bundled face. Bundled for two reasons: a bare machine
    /// then has a real Latin face instead of boxes, and a text-bearing screenshot matrix can pin
    /// its Latin font instead of inheriting whatever the host happens to install (the baseline
    /// guide requires fixed fonts).
    /// </summary>
    public const string LatinFamilyName = "Noto Sans";

    /// <summary>The collection key of the bundled Latin face.</summary>
    public static Uri LatinCollectionKey { get; } = new("fonts:SpotledgerSans");

    /// <summary>The bundled Latin family: the collection only, for the same reason as <see cref="Urdu"/>.</summary>
    public static FontFamily Latin { get; } = new($"{LatinCollectionKey}#{LatinFamilyName}");

    /// <summary>The bundled Latin face.</summary>
    public static Uri LatinRegularAsset { get; } = new(UrduSource, "NotoSans-Regular.ttf");

    /// <summary>The SIL OFL 1.1 text that ships beside the Latin face.</summary>
    public static Uri LatinLicenceAsset { get; } = new(UrduSource, "NotoSans-OFL.txt");

    private static readonly Lock Gate = new();

    /// <summary>
    /// The manager this process registered against. Guarded by identity, not by a bool: Avalonia
    /// headless test sessions can install a new <see cref="FontManager"/> mid-run, and a plain
    /// "already registered" flag then leaves the new manager with no collections while every
    /// later lookup silently returns null.
    /// </summary>
    private static FontManager? _registeredWith;

    /// <summary>True when the collection is present in the current font manager.</summary>
    public static bool IsRegistered
    {
        get
        {
            lock (Gate)
            {
                return ReferenceEquals(_registeredWith, FontManager.Current);
            }
        }
    }

    /// <summary>
    /// Registers the bundled Urdu faces with the process font manager. Idempotent and safe
    /// to call before or after the application starts; call it once per process (the
    /// font manager is process-global, like Avalonia's dispatcher).
    /// </summary>
    public static void Register()
    {
        var manager = FontManager.Current;
        lock (Gate)
        {
            if (ReferenceEquals(_registeredWith, manager))
            {
                return;
            }

            Add(manager, new EmbeddedFontCollection(UrduCollectionKey, UrduSource));
            Add(manager, new EmbeddedFontCollection(LatinCollectionKey, UrduSource));
            _registeredWith = manager;
        }

        static void Add(FontManager manager, EmbeddedFontCollection collection)
        {
            try
            {
                manager.AddFontCollection(collection);
            }
            catch (ArgumentException)
            {
                // A previous registration against this same manager already added the key (for
                // example another copy of this assembly, or a re-entrant call from a host).
            }
        }
    }

    /// <summary>
    /// The glyph typeface the font manager resolves for the bundled Urdu family, or null
    /// when nothing matches. Lets a test or a diagnostic prove the bundled face — not a
    /// host font — is what the pipeline will shape with.
    /// </summary>
    public static GlyphTypeface? ResolveUrduGlyphTypeface(Typeface? typeface = null) =>
        FontManager.Current.TryGetGlyphTypeface(typeface ?? new Typeface(Urdu), out var glyphTypeface)
            ? glyphTypeface
            : null;

    /// <summary>Opens a bundled font asset stream (used by the no-tofu shaping check).</summary>
    public static Stream OpenAsset(Uri asset)
    {
        ArgumentNullException.ThrowIfNull(asset);
        return AssetLoader.Open(asset);
    }
}
