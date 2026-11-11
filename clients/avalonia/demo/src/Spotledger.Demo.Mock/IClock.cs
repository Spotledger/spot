namespace Spotledger.Demo.Mock;

public interface IClock { DateTimeOffset Now { get; } }
/// <summary>Injectable deterministic clock. Never reads the wall clock.</summary>
public sealed class FixedClock(DateTimeOffset now) : IClock
{
    public DateTimeOffset Now { get; private set; } = now;
    public void Advance(TimeSpan delta) => Now = Now.Add(delta);
}
