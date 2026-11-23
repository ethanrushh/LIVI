using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

namespace CarPiHat;

internal enum BrightnessMode { Auto, High, Low, Disabled }

internal sealed record Settings
{
  public double ShutdownDelaySeconds { get; init; } = 40;
  public bool ShutdownEnabled { get; init; }
  public string CanInterface { get; init; } = "can0";
  public BrightnessMode Brightness { get; init; } = BrightnessMode.Auto;
  public string? BacklightDevice { get; init; }
  public int BrightnessHighPercent { get; init; } = 100;
  public int BrightnessLowPercent { get; init; } = 12;
  public string? LiviSocket { get; init; }
  public double WheelVolumeStepPercent { get; init; } = 4;

  public static Settings Load(string path)
  {
    var json = new JsonSerializerOptions
    {
      PropertyNameCaseInsensitive = true,
      UnmappedMemberHandling = JsonUnmappedMemberHandling.Disallow,
      Converters = { new JsonStringEnumConverter<BrightnessMode>(allowIntegerValues: false) },
    };
    var settings = JsonSerializer.Deserialize<Settings>(File.ReadAllText(path), json)
        ?? throw new InvalidDataException("Configuration must be a JSON object.");
    if (!double.IsFinite(settings.ShutdownDelaySeconds) ||
        settings.ShutdownDelaySeconds is < 0 or > 86400)
      throw new InvalidDataException("ShutdownDelaySeconds must be between 0 and 86400.");
    if (settings.CanInterface is null || !Regex.IsMatch(settings.CanInterface, @"\A[A-Za-z0-9_.-]{1,15}\z") ||
        settings.CanInterface is "." or "..")
      throw new InvalidDataException("CanInterface must be a Linux interface name.");
    if (settings.BacklightDevice is { } device &&
        (!Regex.IsMatch(device, @"\A[A-Za-z0-9_.-]+\z") || device is "." or ".."))
      throw new InvalidDataException("BacklightDevice must be a name in /sys/class/backlight.");
    if (settings.BrightnessLowPercent < 1 || settings.BrightnessHighPercent > 100 ||
        settings.BrightnessLowPercent > settings.BrightnessHighPercent)
      throw new InvalidDataException("Brightness levels must be between 1 and 100, with Low at or below High.");
    if (settings.LiviSocket is { } socket && !Path.IsPathFullyQualified(socket))
      throw new InvalidDataException("LiviSocket must be an absolute path.");
    if (!double.IsFinite(settings.WheelVolumeStepPercent) || settings.WheelVolumeStepPercent is < 0 or > 10)
      throw new InvalidDataException("WheelVolumeStepPercent must be between 0 and 10.");
    return settings;
  }
}
