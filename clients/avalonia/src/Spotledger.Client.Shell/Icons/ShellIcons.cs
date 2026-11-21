using Avalonia.Automation;
using Avalonia.Controls;
using Avalonia.Layout;
using Avalonia.Media;

namespace Spotledger.Client.Shell;

/// <summary>
/// The real icons of the sidebar, header and launcher (brief deliverable 5): hand-parsed
/// 24×24 path geometry in the Shell project, because the brief forbids an icon package
/// that would be a second design system. Strokes carry no brush here — the view sets the
/// token brush — so the glyphs follow light/dark and the selected state.
/// </summary>
public static class ShellIcons
{
    /// <summary>The 24×24 geometry for one icon name.</summary>
    public static Geometry Data(string name) => Geometry.Parse(name switch
    {
        "home" => "M3,11 L12,3 L21,11 M5,10 V20 H19 V10 M9,20 V14 H15 V20",
        "tasks" => "M4,7 h16 M4,12 h16 M4,17 h10",
        "selling" => "M4,8 h16 v11 h-16 z M8,8 v-3 h8 v3 M12,12 v4",
        "buying" => "M4,7 h16 l-2,11 h-12 z M8,7 a4,4 0 0 1 8,0",
        "stock" => "M4,4 h7 v7 h-7 z M13,4 h7 v7 h-7 z M4,13 h7 v7 h-7 z M13,13 h7 v7 h-7 z",
        "accounts" => "M3,6 h18 v13 h-18 z M3,10 h18 M15,14 h3",
        "reports" => "M5,3 h11 l4,4 v14 h-15 z M15,3 v5 h5 M8,13 h8 M8,17 h8",
        "settings" => "M12,8 a4,4 0 1 0 0.01,0 M12,2 v3 M12,19 v3 M2,12 h3 M19,12 h3 M5,5 l2,2 M17,17 l2,2 M19,5 l-2,2 M7,17 l-2,2",
        "search" => "M10,4 a6,6 0 1 0 0.01,0 M15,15 L20,20",
        "bell" => "M12,4 a6,6 0 0 1 6,6 v4 l2,2 H4 l2,-2 v-4 a6,6 0 0 1 6,-6 M10,19 a2,2 0 0 0 4,0",
        "plus" => "M12,5 v14 M5,12 h14",
        "close" => "M6,6 L18,18 M18,6 L6,18",
        "pin" => "M12,3 l3,6 4,1 -4,2 0,6 -3,3 -3,-3 0,-6 -4,-2 4,-1 z",
        "menu" => "M4,7 h16 M4,12 h16 M4,17 h16",
        "panel" => "M3,5 h18 v14 h-18 z M9,5 v14",
        "chevron-down" => "M7,10 L12,15 L17,10",
        "chevron-right" => "M10,7 L15,12 L10,17",
        "more" => "M6,12 m-1.1,0 a1.1,1.1 0 1 0 2.2,0 a1.1,1.1 0 1 0 -2.2,0 M12,12 m-1.1,0 a1.1,1.1 0 1 0 2.2,0 a1.1,1.1 0 1 0 -2.2,0 M18,12 m-1.1,0 a1.1,1.1 0 1 0 2.2,0 a1.1,1.1 0 1 0 -2.2,0",
        "user" => "M12,12 a4,4 0 1 0 0.01,0 M5,20 a7,7 0 0 1 14,0",
        "logo" => "M12,3 L20,7 V12 C20,16.5 16.5,19.8 12,21 C7.5,19.8 4,16.5 4,12 V7 Z M9,12 l2,2 4,-4",
        "connection" => "M5,13 a7,7 0 0 1 14,0 M8,15 a4,4 0 0 1 8,0 M12,18 m-1,0 a1,1 0 1 0 2,0 a1,1 0 1 0 -2,0",
        _ => "M5,5 h14 v14 h-14 z",
    });

    /// <summary>A line-glyph control sized for the chrome; the caller styles the stroke brush.</summary>
    public static Avalonia.Controls.Shapes.Path Glyph(string name, double size = 18, string? automationName = null)
    {
        var path = new Avalonia.Controls.Shapes.Path
        {
            Data = Data(name),
            StrokeThickness = 1.6,
            Width = size,
            Height = size,
            Stretch = Stretch.Uniform,
            VerticalAlignment = VerticalAlignment.Center,
            HorizontalAlignment = HorizontalAlignment.Center,
            IsHitTestVisible = false,
        };
        AutomationProperties.SetName(path, automationName ?? name);
        return path;
    }
}
