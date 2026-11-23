using System.Buffers.Binary;
using System.Globalization;
using System.Net;
using System.Net.Sockets;
using System.Threading.Channels;

namespace CarPiHat;

internal enum WheelButton : byte
{
  Released = 0x00,
  Next = 0x02,
  Previous = 0x03,
  VolumeUp = 0x06,
  VolumeDown = 0x07,
  Call = 0x1A,
  Voice = 0x2B,
}

internal sealed record CanFrame(uint Id, byte[] Data);
internal sealed record TelemetryEvent(CanFrame? Frame, bool? Illumination);
internal readonly record struct WheelPress(WheelButton Button, long Timestamp);

internal sealed class VehicleState
{
  private int illumination = -1;
  private WheelButton wheel;
  private readonly Channel<TelemetryEvent> telemetry = Channel.CreateBounded<TelemetryEvent>(
      new BoundedChannelOptions(32) { FullMode = BoundedChannelFullMode.DropOldest, SingleReader = true });
  private readonly Channel<WheelPress> presses = Channel.CreateBounded<WheelPress>(
      new BoundedChannelOptions(16) { FullMode = BoundedChannelFullMode.DropOldest, SingleReader = true });
  private volatile bool liviReady;

  public bool? Illuminated => Volatile.Read(ref illumination) switch { 0 => false, 1 => true, _ => null };
  public ChannelReader<TelemetryEvent> Telemetry => telemetry.Reader;
  public ChannelReader<WheelPress> Presses => presses.Reader;

  public void LiviConnected(bool ready)
  {
    liviReady = ready;
    if (ready && Illuminated is { } lit)
      telemetry.Writer.TryWrite(new TelemetryEvent(null, lit));
  }

  public void Ingest(uint id, ReadOnlySpan<byte> data)
  {
    switch (id)
    {
      case CanReader.WheelId when data.Length >= 1:
        var button = Enum.IsDefined((WheelButton)data[0]) ? (WheelButton)data[0] : WheelButton.Released;
        if (button == wheel)
          return;
        wheel = button;
        telemetry.Writer.TryWrite(new TelemetryEvent(new CanFrame(id, data.ToArray()), null));
        if (button != WheelButton.Released && liviReady)
          presses.Writer.TryWrite(new WheelPress(button, System.Diagnostics.Stopwatch.GetTimestamp()));
        break;

      case CanReader.IlluminationId when data.Length >= 2:
        var lit = data[1] != 0;
        var value = lit ? 1 : 0;
        if (Interlocked.Exchange(ref illumination, value) == value)
          return;
        Log.Write("can", $"Switch illumination {(lit ? "on" : "off")}.");
        telemetry.Writer.TryWrite(new TelemetryEvent(new CanFrame(id, data.ToArray()), lit));
        break;
    }
  }
}

internal sealed class CanReader(string interfaceName, VehicleState vehicle)
{
  public const uint WheelId = 0x5C1;
  public const uint IlluminationId = 0x635;
  private const uint StandardDataFrameMask = 0xC00007FF; // EFF, RTR and the 11-bit ID.
  private const int SolCanRaw = 101;
  private const int CanRawFilter = 1;
  private const int CanFrameSize = 16;
  private readonly string netPath = $"/sys/class/net/{interfaceName}";
  private readonly Health health = new("can");

  public async Task RunAsync(CancellationToken cancellation)
  {
    var frame = new byte[CanFrameSize];
    while (!cancellation.IsCancellationRequested)
    {
      try
      {
        var index = InterfaceIndex();
        using var socket = new Socket(AddressFamily.ControllerAreaNetwork, SocketType.Raw, ProtocolType.Raw);
        var filters = new byte[16];
        BinaryPrimitives.WriteUInt32LittleEndian(filters.AsSpan(0), WheelId);
        BinaryPrimitives.WriteUInt32LittleEndian(filters.AsSpan(4), StandardDataFrameMask);
        BinaryPrimitives.WriteUInt32LittleEndian(filters.AsSpan(8), IlluminationId);
        BinaryPrimitives.WriteUInt32LittleEndian(filters.AsSpan(12), StandardDataFrameMask);
        socket.SetRawSocketOption(SolCanRaw, CanRawFilter, filters);
        socket.Bind(new CanEndPoint(index));
        Log.Write("can", $"Listening for PQ35 wheel and illumination frames on {interfaceName}.");
        health.Ok($"CAN interface {interfaceName} recovered.");

        while (!cancellation.IsCancellationRequested)
        {
          // Periodically verify that an idle interface is still present and up.
          using var receiveTimeout = CancellationTokenSource.CreateLinkedTokenSource(cancellation);
          receiveTimeout.CancelAfter(TimeSpan.FromSeconds(3));
          int size;
          try
          {
            size = await socket.ReceiveAsync(frame.AsMemory(), SocketFlags.None, receiveTimeout.Token);
          }
          catch (OperationCanceledException) when (!cancellation.IsCancellationRequested)
          {
            if (InterfaceIndex() != index)
              throw new IOException("Interface was replaced.");
            continue;
          }
          if (size == CanFrameSize && frame[4] <= 8)
            vehicle.Ingest(BinaryPrimitives.ReadUInt32LittleEndian(frame), frame.AsSpan(8, frame[4]));
        }
      }
      catch (Exception error) when (error is IOException or SocketException or UnauthorizedAccessException)
      {
        health.Fail($"Cannot receive on {interfaceName}. Retrying. {error.Message}");
        await Task.Delay(TimeSpan.FromSeconds(2), cancellation);
      }
    }
  }

  private int InterfaceIndex()
  {
    var flags = File.ReadAllText(Path.Combine(netPath, "flags")).Trim();
    if (!flags.StartsWith("0x", StringComparison.Ordinal) ||
        !uint.TryParse(flags.AsSpan(2), NumberStyles.HexNumber, CultureInfo.InvariantCulture, out var value) ||
        (value & 1) == 0) // IFF_UP
      throw new IOException($"Interface {interfaceName} is down.");
    if (!int.TryParse(File.ReadAllText(Path.Combine(netPath, "ifindex")), out var index) || index <= 0)
      throw new IOException($"Interface {interfaceName} has an invalid index.");
    return index;
  }

  private sealed class CanEndPoint(int index) : EndPoint
  {
    public override AddressFamily AddressFamily => AddressFamily.ControllerAreaNetwork;

    public override SocketAddress Serialize()
    {
      // struct sockaddr_can on Linux. SocketAddress writes the native family field.
      var address = new SocketAddress(AddressFamily, size: 16);
      BinaryPrimitives.WriteInt32LittleEndian(address.Buffer.Span[4..], index);
      return address;
    }

    public override EndPoint Create(SocketAddress address) =>
        new CanEndPoint(BinaryPrimitives.ReadInt32LittleEndian(address.Buffer.Span[4..]));
  }
}
