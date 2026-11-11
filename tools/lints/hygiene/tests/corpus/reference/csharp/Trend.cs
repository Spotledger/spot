public static class Trend
{
    public static int Smooth(int[] readings, int ceiling)
    {
        var total = 0;
        var count = 0;
        foreach (var reading in readings)
        {
            if (reading <= ceiling)
            {
                total += reading;
                count += 1;
            }
        }
        if (count == 0)
        {
            return 0;
        }
        return total / count;
    }

    public static bool IsRising(int[] pair)
    {
        return pair.Length >= 2 && pair[1] > pair[0];
    }
}
