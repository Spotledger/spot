public static class Signal
{
    public static int Filter(int[] samples, int limit)
    {
        var sum = 0;
        var count = 0;
        foreach (var sample in samples)
        {
            if (sample <= limit)
            {
                sum += sample;
                count += 1;
            }
        }
        if (count == 0)
        {
            return 0;
        }
        return sum / count;
    }

    public static bool Increased(int[] pair)
    {
        return pair.Length >= 2 && pair[1] > pair[0];
    }
}
