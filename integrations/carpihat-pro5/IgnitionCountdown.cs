namespace CarPiHat;

internal readonly record struct CountdownUpdate(
    bool Started = false, bool Expired = false, bool Interrupted = false, TimeSpan? Remaining = null);

internal sealed class IgnitionCountdown(TimeSpan delay)
{
  private static readonly TimeSpan Debounce = TimeSpan.FromMilliseconds(250);
  private static readonly TimeSpan MaxSampleGap = TimeSpan.FromSeconds(1);
  private TimeSpan? lastSample;
  private TimeSpan? lowSince;
  private TimeSpan? deadline;
  private bool expired;

  public CountdownUpdate Sample(bool? high, TimeSpan now)
  {
    var gap = lastSample is { } last && now - last > MaxSampleGap;
    lastSample = now;
    if (high is not false || gap)
    {
      var remaining = Stop(now);
      if (high is false)
        lowSince = now;
      return new CountdownUpdate(Interrupted: true, Remaining: remaining);
    }

    if (expired)
      return default;
    lowSince ??= now;
    if (now - lowSince < Debounce)
      return default;

    var started = deadline is null;
    deadline ??= lowSince.Value + delay;
    var left = deadline.Value - now;
    if (left > TimeSpan.Zero)
      return new CountdownUpdate(Started: started, Remaining: left);

    deadline = null;
    expired = true;
    return new CountdownUpdate(Started: started, Expired: true, Remaining: TimeSpan.Zero);
  }

  public TimeSpan? Stop(TimeSpan now)
  {
    TimeSpan? remaining = deadline is { } end ? TimeSpan.FromTicks(Math.Max(0, (end - now).Ticks)) : null;
    deadline = null;
    lowSince = null;
    expired = false;
    return remaining;
  }
}
