using System.Globalization;

namespace CarPiHat;

internal sealed class Backlight(Settings settings, VehicleState vehicle)
{
  private const string Backlights = "/sys/class/backlight";

  public async Task RunAsync(CancellationToken cancellation)
  {
    if (settings.Brightness == BrightnessMode.Disabled)
      return;

    using var timer = new PeriodicTimer(TimeSpan.FromSeconds(1));
    var health = new Health("backlight");
    (string Device, int Level)? logged = null;
    do
    {
      var percent = settings.Brightness switch
      {
        BrightnessMode.High => settings.BrightnessHighPercent,
        BrightnessMode.Low => settings.BrightnessLowPercent,
        _ => vehicle.Illuminated switch
        {
          true => settings.BrightnessLowPercent,
          false => settings.BrightnessHighPercent,
          null => (int?)null,
        },
      };
      if (percent is null)
        continue;

      try
      {
        var device = ChooseDevice();
        var maximum = await ReadLevel(Path.Combine(device, "max_brightness"), cancellation);
        if (maximum < 1)
          throw new IOException("Backlight reported an invalid maximum.");
        var level = Math.Clamp((int)Math.Round(maximum * (percent.Value / 100.0)), 1, maximum);
        var brightness = Path.Combine(device, "brightness");
        if (await ReadLevel(brightness, cancellation) != level)
          await File.WriteAllTextAsync(brightness, level.ToString(CultureInfo.InvariantCulture) + "\n", cancellation);
        health.Ok("Backlight is available again.");
        if (logged != (device, level))
        {
          Log.Write("backlight", $"{Path.GetFileName(device)} set to {level}/{maximum} ({percent}%).");
          logged = (device, level);
        }
      }
      catch (Exception error) when (error is IOException or UnauthorizedAccessException or FormatException)
      {
        health.Fail($"Cannot set backlight. Retrying. {error.Message}");
      }
    } while (await timer.WaitForNextTickAsync(cancellation));
  }

  private string ChooseDevice()
  {
    if (settings.BacklightDevice is { } name)
      return Path.Combine(Backlights, name);
    var devices = Directory.GetDirectories(Backlights);
    return devices.Length switch
    {
      1 => devices[0],
      0 => throw new IOException("No backlight device is present yet."),
      _ => throw new IOException("Multiple backlights found. Set BacklightDevice in configuration."),
    };
  }

  private static async Task<int> ReadLevel(string path, CancellationToken cancellation) =>
      int.Parse(await File.ReadAllTextAsync(path, cancellation), CultureInfo.InvariantCulture);
}
