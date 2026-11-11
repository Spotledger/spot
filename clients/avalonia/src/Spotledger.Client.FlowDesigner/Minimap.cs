using System;
using System.Collections.Generic;

namespace Spotledger.Client.FlowDesigner;

/// The minimap's projection: it fits the whole drawing into a small box, draws the viewport as a
/// frame, and maps a click back to designer space so a click in the overview jumps the canvas.
/// Pure arithmetic, so a test can check the round trip exactly.
public sealed class MinimapProjection
{

    private MinimapProjection(
        double contentWidth,
        double contentHeight,
        double scale,
        double offsetX,
        double offsetY,
        double width,
        double height)
    {
        ContentWidth = contentWidth;
        ContentHeight = contentHeight;
        Scale = scale;
        OffsetX = offsetX;
        OffsetY = offsetY;
        Width = width;
        Height = height;
    }

    public double ContentWidth { get; }

    public double ContentHeight { get; }

    public double Scale { get; }

    public double OffsetX { get; }

    public double OffsetY { get; }

    public double Width { get; }

    public double Height { get; }

    public static MinimapProjection For(FlowDocument document, double width, double height, double padding = 6)
    {
        ArgumentNullException.ThrowIfNull(document);
        if (width <= 0 || height <= 0)
        {
            throw new ArgumentOutOfRangeException(nameof(width), "The minimap needs a positive size.");
        }

        double contentWidth = 0;
        double contentHeight = 0;
        foreach (FlowNode node in document.Nodes)
        {
            contentWidth = Math.Max(contentWidth, node.Position.X + FlowLayoutEngine.NodeWidth);
            contentHeight = Math.Max(contentHeight, node.Position.Y + FlowLayoutEngine.NodeHeight);
        }

        contentWidth = Math.Max(contentWidth, 1);
        contentHeight = Math.Max(contentHeight, 1);
        double usableWidth = Math.Max(width - (2 * padding), 1);
        double usableHeight = Math.Max(height - (2 * padding), 1);
        double scale = Math.Max(contentWidth / usableWidth, contentHeight / usableHeight);

        // Centre the drawing in the box: the leftover space becomes the margin.
        double offsetX = (width - (contentWidth / scale)) / 2;
        double offsetY = (height - (contentHeight / scale)) / 2;
        return new MinimapProjection(contentWidth, contentHeight, scale, offsetX, offsetY, width, height);
    }

    public FlowPoint ToMinimap(FlowPoint world) =>
        new(OffsetX + (world.X / Scale), OffsetY + (world.Y / Scale));

    public FlowPoint ToWorld(FlowPoint minimap) =>
        new((minimap.X - OffsetX) * Scale, (minimap.Y - OffsetY) * Scale);

    public (FlowPoint TopLeft, FlowPoint Size) ViewportFrame(
        double panX,
        double panY,
        double zoom,
        double viewportWidth,
        double viewportHeight)
    {
        if (zoom <= 0)
        {
            throw new ArgumentOutOfRangeException(nameof(zoom), "Zoom must be positive.");
        }

        FlowPoint topLeft = ToMinimap(new FlowPoint(-panX / zoom, -panY / zoom));
        FlowPoint size = new(viewportWidth / zoom / Scale, viewportHeight / zoom / Scale);
        return (topLeft, size);
    }

    public IReadOnlyList<(string Id, FlowPoint TopLeft, FlowPoint Size)> Nodes(FlowDocument document)
    {
        ArgumentNullException.ThrowIfNull(document);
        List<(string, FlowPoint, FlowPoint)> boxes = new(document.Nodes.Count);
        foreach (FlowNode node in document.Nodes)
        {
            boxes.Add((
                node.Id,
                ToMinimap(node.Position),
                new FlowPoint(FlowLayoutEngine.NodeWidth / Scale, FlowLayoutEngine.NodeHeight / Scale)));
        }

        return boxes;
    }
}
