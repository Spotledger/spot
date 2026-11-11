using Avalonia;
using Avalonia.Controls;
using Avalonia.Media;
using Spotledger.Client.Theme;

namespace Spotledger.Client.Notifications;

/// <summary>Shared look helpers for the views: theme tokens, severity shape and relative time.</summary>
internal static class NoticeLook
{
    /// <summary>The token that colours a severity.</summary>
    public static string ColourToken(NoticeSeverity severity) => severity switch
    {
        NoticeSeverity.Success => Tokens.TokenColorSemanticSuccess,
        NoticeSeverity.Warning => Tokens.TokenColorSemanticWarning,
        NoticeSeverity.Error => Tokens.TokenColorSemanticDanger,
        _ => Tokens.TokenColorSemanticInfo,
    };

    /// <summary>A shape per severity so colour is never the only signal: circle, check, triangle, cross.</summary>
    public static string Shape(NoticeSeverity severity) => severity switch
    {
        NoticeSeverity.Success => "M5,12 L10,17 L19,7",
        NoticeSeverity.Warning => "M12,4 L21,20 H3 Z",
        NoticeSeverity.Error => "M6,6 L18,18 M18,6 L6,18",
        _ => "M12,4 a8,8 0 1 0 0.01,0",
    };

    /// <summary>The spoken name of a severity.</summary>
    public static string Name(NoticeSeverity severity) => severity.ToString();

    /// <summary>A short relative time from an injected clock: "now", "5 min ago", "3 h ago", "2 d ago".</summary>
    public static string Relative(DateTimeOffset at, DateTimeOffset now)
    {
        var age = now - at;
        if (age < TimeSpan.FromMinutes(1))
        {
            return "now";
        }

        if (age < TimeSpan.FromHours(1))
        {
            return $"{(int)age.TotalMinutes} min ago";
        }

        return age < TimeSpan.FromDays(1) ? $"{(int)age.TotalHours} h ago" : $"{(int)age.TotalDays} d ago";
    }

    /// <summary>Binds a control property to a theme colour token (re-applied when the theme changes).</summary>
    public static IDisposable BindBrush(Control control, string token, Action<IBrush?> apply) =>
        control.GetResourceObservable(token).Subscribe(new Avalonia.Reactive.AnonymousObserver<object?>(value =>
            apply(value switch { Color color => new SolidColorBrush(color), IBrush brush => brush, _ => null })));

    /// <summary>A plain, bounded text block: never markup.</summary>
    public static TextBlock Text(string text, int maxLines, double? size = null)
    {
        var block = new TextBlock { Text = text, TextWrapping = TextWrapping.Wrap, MaxLines = maxLines, TextTrimming = TextTrimming.CharacterEllipsis };
        if (size is { } fontSize)
        {
            block.FontSize = fontSize;
        }

        return block;
    }
}
