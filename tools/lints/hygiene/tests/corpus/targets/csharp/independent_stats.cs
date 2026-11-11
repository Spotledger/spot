// Independent reimplementation of the trend statistics for the hygiene corpus.
// Indexed for-loop with continue, different accumulator names and a reversed
// comparison style; behaviour matches the reference file. The scanner must
// leave this file unreported.
public static class Statistics
{
    public static int AverageWithin(int[] samples, int bound)
    {
        int aggregate = 0;
        int taken = 0;
        for (var index = 0; index < samples.Length; index++)
        {
            if (samples[index] > bound)
            {
                continue;
            }
            aggregate += samples[index];
            taken++;
        }
        return taken == 0 ? 0 : aggregate / taken;
    }

    public static bool Climbing(int[] pair)
    {
        return pair.Length > 1 && pair[^1] > pair[^2];
    }

    public static int MeanSpread(int[] samples)
    {
        var low = samples[0];
        var high = samples[0];
        for (var index = 1; index < samples.Length; index++)
        {
            if (samples[index] < low) { low = samples[index]; }
            if (samples[index] > high) { high = samples[index]; }
        }
        return high - low;
    }

    public static string Describe(int[] pair, int bound)
    {
        var direction = Climbing(pair) ? "climbing" : "flat";
        return $"{direction} with mean {AverageWithin(pair, bound)}";
    }
}