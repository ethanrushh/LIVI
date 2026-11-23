using System.Buffers.Binary;
using System.ComponentModel;
using System.Device.Gpio;
using System.Device.Gpio.Drivers;
using System.Diagnostics;

namespace CarPiHat;

internal sealed class PowerMonitor(Settings settings)
{
  private const int Ignition = 12;
  private const int Dsp = 22;
  private const int Latch = 25;
  private static readonly TimeSpan Poll = TimeSpan.FromMilliseconds(100);
  private readonly TimeSpan delay = TimeSpan.FromSeconds(settings.ShutdownDelaySeconds);
  private readonly Health ignitionHealth = new("ignition");
  private readonly Health latchHealth = new("latch");
  private readonly Health dspHealth = new("dsp");
  private readonly Health poweroffHealth = new("poweroff");
  private TimeSpan? retryPoweroff;

  public async Task RunAsync(CancellationToken cancellation)
  {
    using var timer = new PeriodicTimer(Poll);
    var clock = Stopwatch.StartNew();
    var countdown = new IgnitionCountdown(delay);
    GpioController? gpio = null;
    var kernelLatch = false;
    var latchReady = false;
    var dspReady = false;
    try
    {
      do
      {
        if (gpio is null)
        {
          try
          {
            var chip = LibGpiodV2Driver.GetAvailableChips()
                .SingleOrDefault(chip =>
                    chip.Label == "pinctrl-rp1" && chip.Name == $"gpiochip{chip.Id}");
            if (chip is null)
              throw new IOException("The Pi 5 RP1 GPIO chip is unavailable.");
            gpio = new GpioController(new LibGpiodV2Driver(chip));
            kernelLatch = HasKernelLatch();
            if (kernelLatch)
              Log.Write("latch", "BCM25 is held high by the kernel poweroff driver.");
          }
          catch (Exception error) when (IsGpioError(error))
          {
            ignitionHealth.Fail($"GPIO unavailable. Retrying. {error.Message}");
            await Task.Delay(TimeSpan.FromSeconds(2), cancellation);
            continue;
          }
        }

        // Assert the latch before the DSP. Keep pins open throughout the service lifetime.
        latchReady = latchReady || kernelLatch || HoldHigh(gpio, Latch, latchHealth);
        dspReady = dspReady || HoldHigh(gpio, Dsp, dspHealth);

        var now = clock.Elapsed;
        var high = ReadIgnition(gpio);
        var update = countdown.Sample(high, now);
        if (update.Interrupted)
        {
          LogEnd(update.Remaining, high switch
          {
            true => "ignition on",
            false => "ignition sampling interrupted",
            null => "ignition unreadable",
          });
          retryPoweroff = null;
        }
        if (update.Started)
          Log.Write("ignition", $"Countdown started with {Seconds(update.Remaining ?? TimeSpan.Zero)} seconds remaining (ignition off).");
        if (update.Expired)
        {
          var reason = settings.ShutdownEnabled ? "expired" : "expired, poweroff disabled";
          LogEnd(TimeSpan.Zero, reason);
          if (settings.ShutdownEnabled)
            retryPoweroff = now;
        }

        if (high is false && retryPoweroff is { } retry && now >= retry)
        {
          // Recheck immediately before asking systemd, independently of the last poll.
          if (ReadIgnition(gpio) is not false)
          {
            countdown.Stop(clock.Elapsed);
            retryPoweroff = null;
            continue;
          }

          var latchCanRelease = kernelLatch && HasKernelLatch();
          if (latchCanRelease && await RequestPoweroffAsync(cancellation))
            retryPoweroff = null;
          else
          {
            if (!latchCanRelease)
              poweroffHealth.Fail("Poweroff blocked until the active-low BCM25 kernel latch is configured.");
            retryPoweroff = clock.Elapsed + TimeSpan.FromSeconds(10);
          }
        }
      } while (await timer.WaitForNextTickAsync(cancellation));
    }
    finally
    {
      LogEnd(countdown.Stop(clock.Elapsed), "service stopping");
      gpio?.Dispose();
    }
  }

  private bool? ReadIgnition(GpioController gpio)
  {
    try
    {
      if (!gpio.IsPinOpen(Ignition))
        gpio.OpenPin(Ignition, PinMode.Input);
      var high = gpio.Read(Ignition) == PinValue.High;
      ignitionHealth.Ok("BCM12 is readable again.");
      return high;
    }
    catch (Exception error) when (IsGpioError(error))
    {
      ignitionHealth.Fail($"Cannot read BCM12. Countdown is inhibited. {error.Message}");
      CloseIfOpen(gpio, Ignition);
      return null;
    }
  }

  private static bool HoldHigh(GpioController gpio, int pin, Health health)
  {
    try
    {
      // The span overload goes directly to the v2 driver. It requests output and initial
      // high together on a free line and retains the request until the driver is disposed.
      gpio.Write([new PinValuePair(pin, PinValue.High)]);
      Log.Write(pin == Latch ? "latch" : "dsp", $"Holding BCM{pin} high.");
      return true;
    }
    catch (Exception error) when (IsGpioError(error))
    {
      health.Fail($"Cannot hold BCM{pin} high. Retrying. {error.Message}");
      return false;
    }
  }

  private static void CloseIfOpen(GpioController gpio, int pin)
  {
    try
    {
      if (gpio.IsPinOpen(pin))
        gpio.ClosePin(pin);
    }
    catch (Exception error) when (IsGpioError(error))
    {
      // A line can disappear while it is being released. Retry on the next poll.
    }
  }

  private static void LogEnd(TimeSpan? remaining, string reason)
  {
    if (remaining is { } time)
      Log.Write("ignition", $"Countdown stopped with {Seconds(time)} seconds remaining ({reason}).");
  }

  private static string Seconds(TimeSpan duration) => Math.Max(0, duration.TotalSeconds).ToString("0.0");

  private static bool IsGpioError(Exception error) => error is IOException or UnauthorizedAccessException
      or InvalidOperationException or DllNotFoundException or Win32Exception or NotSupportedException
      or GpiodException;

  internal static bool HasKernelLatch()
  {
    const string device = "/sys/bus/platform/devices/power_ctrl";
    try
    {
      var driver = new DirectoryInfo(Path.Combine(device, "driver")).LinkTarget;
      if (Path.GetFileName(driver) != "poweroff-gpio" || File.Exists($"{device}/of_node/input"))
        return false;

      // gpio-poweroff uses a GPIO phandle followed by the BCM offset and flags.
      var cells = File.ReadAllBytes($"{device}/of_node/gpios");
      return cells.Length == 12 && BinaryPrimitives.ReadUInt32BigEndian(cells.AsSpan(4)) == Latch &&
          BinaryPrimitives.ReadUInt32BigEndian(cells.AsSpan(8)) == 1; // GPIO_ACTIVE_LOW
    }
    catch (Exception error) when (error is IOException or UnauthorizedAccessException)
    {
      return false;
    }
  }

  private async Task<bool> RequestPoweroffAsync(CancellationToken cancellation)
  {
    var start = new ProcessStartInfo("/usr/bin/sudo")
    {
      UseShellExecute = false,
      RedirectStandardError = true,
      ArgumentList = { "-n", "/usr/bin/systemctl", "--no-block", "poweroff" },
    };
    try
    {
      using var process = Process.Start(start) ?? throw new IOException("Could not start systemctl.");
      try
      {
        await process.WaitForExitAsync(cancellation).WaitAsync(TimeSpan.FromSeconds(5), cancellation);
      }
      catch (Exception error) when (error is TimeoutException or OperationCanceledException)
      {
        if (!process.HasExited)
          process.Kill(entireProcessTree: true);
        throw;
      }
      if (process.ExitCode != 0)
        throw new IOException((await process.StandardError.ReadToEndAsync(cancellation)).Trim());
      Log.Write("poweroff", "Systemd accepted the graceful poweroff request.");
      return true;
    }
    catch (Exception error) when (error is IOException or Win32Exception or TimeoutException)
    {
      poweroffHealth.Fail($"Poweroff request failed. Retrying while ignition stays off. {error.Message}");
      return false;
    }
  }
}
