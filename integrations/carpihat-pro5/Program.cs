using System.Runtime.InteropServices;
using System.Text.Json;

namespace CarPiHat;

internal static class Program
{
  public static async Task<int> Main(string[] args)
  {
    if (args is not [var configPath] || !OperatingSystem.IsLinux())
    {
      Console.Error.WriteLine("Usage on Linux: CarPiHat /path/to/config.json");
      return 2;
    }

    Settings settings;
    try
    {
      settings = Settings.Load(configPath);
    }
    catch (Exception error) when (error is IOException or InvalidDataException or JsonException or UnauthorizedAccessException)
    {
      Log.Write("config", error.Message);
      return 2;
    }

    using var stopping = new CancellationTokenSource();
    using var terminate = PosixSignalRegistration.Create(PosixSignal.SIGTERM, signal =>
    {
      signal.Cancel = true;
      stopping.Cancel();
    });
    using var interrupt = PosixSignalRegistration.Create(PosixSignal.SIGINT, signal =>
    {
      signal.Cancel = true;
      stopping.Cancel();
    });
    var cancellation = stopping.Token;
    var vehicle = new VehicleState();
    var livi = new LiviBridge(settings, vehicle);
    Log.Write("service", $"Starting with CAN {settings.CanInterface}, brightness {settings.Brightness}, " +
        $"and poweroff {(settings.ShutdownEnabled ? "enabled" : "disabled (testing)")}.");
    var results = await Task.WhenAll(
        RunWorkerAsync("power", new PowerMonitor(settings).RunAsync, stopping),
        RunWorkerAsync("can", new CanReader(settings.CanInterface, vehicle).RunAsync, stopping),
        RunWorkerAsync("backlight", new Backlight(settings, vehicle).RunAsync, stopping),
        RunWorkerAsync("livi", livi.RunControlsAsync, stopping),
        RunWorkerAsync("telemetry", livi.RunTelemetryAsync, stopping));
    Log.Write("service", "Stopped.");
    return results.All(ok => ok) ? 0 : 1;
  }

  private static async Task<bool> RunWorkerAsync(
      string name, Func<CancellationToken, Task> run, CancellationTokenSource stopping)
  {
    try
    {
      await run(stopping.Token);
      return true;
    }
    catch (OperationCanceledException) when (stopping.IsCancellationRequested)
    {
      return true;
    }
    catch (Exception error)
    {
      Log.Write(name, $"Worker stopped unexpectedly: {error}");
      stopping.Cancel();
      return false;
    }
  }
}

internal static class Log
{
  public static void Write(string area, string message) => Console.WriteLine($"[{area}] {message}");
}

// Log the beginning of a failure and its recovery, even if retrying many times.
internal sealed class Health(string area)
{
  private bool failed;

  public void Fail(string message)
  {
    if (!failed)
      Log.Write(area, message);
    failed = true;
  }

  public void Ok(string message)
  {
    if (failed)
      Log.Write(area, message);
    failed = false;
  }
}
