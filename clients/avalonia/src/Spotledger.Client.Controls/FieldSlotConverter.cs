using System;
using System.Collections.Generic;
using System.Globalization;
using Avalonia.Data.Converters;
using Avalonia.Markup.Xaml;

namespace Spotledger.Client.Controls;

/// <summary>Markup extension: <c>{controls:FieldSlot ForNarrowSlot=True}</c>.</summary>
public sealed class FieldSlotExtension : MarkupExtension
{
    public bool ForNarrowSlot { get; set; } = true;

    public override object ProvideValue(IServiceProvider serviceProvider) =>
        new FieldSlotConverter { ForNarrowSlot = ForNarrowSlot };
}

/// <summary>
/// MultiBinding helper for the SpotField template: passes the field content through only
/// when the wide/narrow state matches the slot the converter is installed in. Both content
/// presenters exist in the template simultaneously; without this gate Avalonia would try to
/// parent the same control twice ("already has a visual parent") on the first measure.
/// </summary>
public sealed class FieldSlotConverter : IMultiValueConverter
{
    /// <summary>True for the stacked (narrow) slot, false for the inline (wide) slot.</summary>
    public bool ForNarrowSlot { get; set; }

    public object? Convert(IList<object?> values, Type targetType, object? parameter, CultureInfo culture)
    {
        if (values.Count != 2 || values[0] is not bool isWide)
        {
            return null;
        }

        return ForNarrowSlot == !isWide ? values[1] : null;
    }
}
