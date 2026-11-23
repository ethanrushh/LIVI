using System.Buffers.Binary;
using System.Diagnostics;
using System.Net.Http.Json;
using System.Net.Sockets;
using System.Text.Json;

namespace CarPiHat;

internal sealed class LiviBridge(Settings settings, VehicleState vehicle)
{
  private static readonly TimeSpan Retry = TimeSpan.FromSeconds(2);
  private readonly string socketPath = settings.LiviSocket ??
      Path.Combine(Environment.GetEnvironmentVariable("XDG_RUNTIME_DIR") ?? "/run/user/1000", "livi", "core.sock");

  public async Task RunControlsAsync(CancellationToken cancellation)
  {
    var health = new Health("livi");
    while (!cancellation.IsCancellationRequested)
    {
      try
      {
        using var socket = new Socket(AddressFamily.Unix, SocketType.Stream, ProtocolType.Unspecified);
        await socket.ConnectAsync(new UnixDomainSocketEndPoint(socketPath), cancellation);
        using var stream = new NetworkStream(socket);
        var session = new LiviSession(stream, cancellation);
        await session.SendAsync(new { type = "hello", protocol = 11, client = "CarPiHAT Pro 5" });
        using var welcome = await session.ReadUntilAsync("welcome");
        var bindings = welcome.RootElement.GetProperty("state").GetProperty("config").GetProperty("bindings");
        var callKey = bindings.GetProperty("acceptPhone").GetString();
        var voiceKey = bindings.GetProperty("voiceAssistant").GetString();
        while (vehicle.Presses.TryRead(out _)) { }
        vehicle.LiviConnected(true);
        health.Ok("LIVI control socket recovered.");
        Log.Write("livi", $"Connected to {socketPath}.");
        await ForwardButtonsAsync(session, callKey, voiceKey, cancellation);
      }
      catch (Exception error) when (error is IOException or SocketException or TimeoutException or
          JsonException or KeyNotFoundException or InvalidOperationException)
      {
        health.Fail($"Control socket unavailable. Buttons are discarded until LIVI reconnects. {error.Message}");
      }
      finally
      {
        vehicle.LiviConnected(false);
      }
      await Task.Delay(Retry, cancellation);
    }
  }

  private async Task ForwardButtonsAsync(
      LiviSession session, string? callKey, string? voiceKey, CancellationToken cancellation)
  {
    var buttonReady = vehicle.Presses.WaitToReadAsync(cancellation).AsTask();
    while (!cancellation.IsCancellationRequested)
    {
      await Task.WhenAny(session.Pending, buttonReady);
      if (session.Pending.IsCompleted)
      {
        using var update = await session.ReadAsync(); // Drain unrelated LIVI state patches.
      }
      if (!buttonReady.IsCompleted)
        continue;

      if (!await buttonReady)
        return;
      if (vehicle.Presses.TryRead(out var press) && Stopwatch.GetElapsedTime(press.Timestamp) < TimeSpan.FromSeconds(1))
      {
        switch (press.Button)
        {
          case WheelButton.Previous:
          case WheelButton.Next:
            var track = press.Button == WheelButton.Next ? "next" : "prev";
            await session.ActionAsync(new { kind = "media", control = track });
            break;
          case WheelButton.VolumeUp:
          case WheelButton.VolumeDown:
            await StepVolumeAsync(session, press.Button == WheelButton.VolumeUp);
            break;
          case WheelButton.Call:
          case WheelButton.Voice:
            var code = press.Button == WheelButton.Call ? callKey : voiceKey;
            if (!string.IsNullOrEmpty(code))
            {
              await session.SendAsync(new { type = "input", input = new { kind = "key", code, down = true } });
              await session.SendAsync(new { type = "input", input = new { kind = "key", code, down = false } });
            }
            break;
        }
      }

      buttonReady = vehicle.Presses.WaitToReadAsync(cancellation).AsTask();
    }
  }

  private async Task StepVolumeAsync(LiviSession session, bool increase)
  {
    if (settings.WheelVolumeStepPercent == 0)
      return;
    // Read the head unit's current master level on each press. Avoid cached levels after user
    // changes from CarPlay or the mixer, and avoid retrying a press with uncertain delivery.
    await session.SendAsync(new { type = "resync" });
    using var sync = await session.ReadUntilAsync("welcome");
    var volume = sync.RootElement.GetProperty("state").GetProperty("config").GetProperty("huVolume");
    if (!volume.TryGetDouble(out var current) || !double.IsFinite(current) || current is < 0 or > 1)
      throw new IOException("LIVI did not report a valid master volume.");
    var step = settings.WheelVolumeStepPercent / 100 * (increase ? 1 : -1);
    var level = Math.Clamp(current + step, 0, 1);
    if (level != current)
      await session.ActionAsync(new { kind = "setConfig", patch = new { huVolume = level } });
  }

  public async Task RunTelemetryAsync(CancellationToken cancellation)
  {
    using var http = new HttpClient(new SocketsHttpHandler { UseProxy = false })
    {
      BaseAddress = new Uri("http://127.0.0.1:4000"),
      Timeout = TimeSpan.FromSeconds(1),
    };
    var health = new Health("telemetry");
    while (!cancellation.IsCancellationRequested)
    {
      var update = await vehicle.Telemetry.ReadAsync(cancellation);
      for (; ; )
      {
        try
        {
          var payload = new Dictionary<string, object>();
          if (update.Frame is { } frame)
            payload["can"] = new { id = frame.Id, data = Array.ConvertAll(frame.Data, value => (int)value) };
          if (update.Illumination is not null && vehicle.Illuminated is { } illuminated)
            payload["nightMode"] = illuminated;
          using var response = await http.PostAsJsonAsync("/telemetry", payload, cancellation);
          response.EnsureSuccessStatusCode();
          health.Ok("LIVI telemetry recovered.");
          break;
        }
        catch (Exception error) when (!cancellation.IsCancellationRequested &&
            error is HttpRequestException or TaskCanceledException)
        {
          health.Fail($"LIVI telemetry unavailable. Retrying with the latest state. {error.Message}");
          await Task.Delay(Retry, cancellation);
          while (vehicle.Telemetry.TryRead(out var latest))
            update = latest;
          // Resend the current illumination even if a newer wheel event replaced it.
          update = new TelemetryEvent(update.Frame, vehicle.Illuminated);
        }
      }
    }
  }

  private sealed class LiviSession(NetworkStream stream, CancellationToken stopping)
  {
    private const int MaxFrame = 16 * 1024 * 1024;
    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(2);
    private Task<JsonDocument>? pending;
    private uint actionId;

    public Task<JsonDocument> Pending => pending ??= ReadFrameAsync();

    public async Task<JsonDocument> ReadAsync()
    {
      var message = await Pending;
      pending = null;
      return message;
    }

    public async Task SendAsync<T>(T value)
    {
      var json = JsonSerializer.SerializeToUtf8Bytes(value);
      var frame = new byte[4 + json.Length];
      BinaryPrimitives.WriteInt32LittleEndian(frame, json.Length);
      json.CopyTo(frame, 4);
      using var timeout = CancellationTokenSource.CreateLinkedTokenSource(stopping);
      timeout.CancelAfter(RequestTimeout);
      try
      {
        await stream.WriteAsync(frame, timeout.Token);
      }
      catch (OperationCanceledException) when (!stopping.IsCancellationRequested)
      {
        throw new TimeoutException("LIVI control write timed out.");
      }
    }

    private async Task<JsonDocument> ReadFrameAsync()
    {
      var header = new byte[4];
      await stream.ReadExactlyAsync(header, stopping);
      var length = BinaryPrimitives.ReadInt32LittleEndian(header);
      if (length is < 1 or > MaxFrame)
        throw new IOException($"LIVI sent an invalid frame size ({length}).");
      var body = new byte[length];
      await stream.ReadExactlyAsync(body, stopping);
      return JsonDocument.Parse(body);
    }

    public async Task<JsonDocument> ReadUntilAsync(string type, uint? id = null)
    {
      using var timeout = CancellationTokenSource.CreateLinkedTokenSource(stopping);
      timeout.CancelAfter(RequestTimeout);
      try
      {
        for (; ; )
        {
          var packet = await ReadAsync().WaitAsync(timeout.Token);
          var message = packet.RootElement;
          var messageType = message.GetProperty("type").GetString();
          if (messageType == type && (id is null || message.GetProperty("id").GetUInt32() == id))
            return packet;
          if (messageType == "refused")
          {
            var reason = message.GetProperty("reason").GetString();
            packet.Dispose();
            throw new IOException($"LIVI rejected the control connection: {reason}");
          }
          packet.Dispose();
        }
      }
      catch (OperationCanceledException) when (!stopping.IsCancellationRequested)
      {
        throw new TimeoutException($"Timed out waiting for a LIVI {type} message.");
      }
    }

    public async Task ActionAsync<T>(T action)
    {
      var id = ++actionId;
      await SendAsync(new { type = "action", id, action });
      using var reply = await ReadUntilAsync("reply", id);
      if (reply.RootElement.TryGetProperty("error", out var error))
        throw new IOException($"LIVI action failed: {error.GetString()}");
    }
  }
}
