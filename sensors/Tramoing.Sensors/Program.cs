using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text.Json;
using LibreHardwareMonitor.Hardware;
using Microsoft.Win32;
using Microsoft.Win32.SafeHandles;
using Tramoing.Sensors;

// Only the elevated helper launches this executable. No command line paths,
// plugins, fan controls, driver installation or network listeners are exposed.
internal static class Program
{
    private const string Library = "LibreHardwareMonitor 0.9.6 / PawnIO";

    private static int Main()
    {
        // The helper assigns our process to a kill-on-close job before sending
        // this handshake. Do not open any sensor hardware before then.
        if (Console.ReadLine() != "start") return 2;
        var output = Console.OpenStandardOutput();
        Console.SetOut(TextWriter.Null); // library diagnostics cannot corrupt the binary stream
        Computer? computer = null;
        try
        {
            while (true)
            {
                try
                {
                    string? failure = CheckPlatformAndDriver();
                    if (failure != null)
                    {
                        computer?.Close();
                        computer = null;
                        Emit(output, new(null, null, failure, FailureDetail(failure)));
                    }
                    else
                    {
                        if (computer == null)
                        {
                            Emit(output, new(null, null, "starting", Library + ": opening CPU sensors"));
                            computer = new Computer { IsCpuEnabled = true };
                            computer.Open();
                        }
                        var temperatures = new List<(string Name, double? Value)>();
                        foreach (var cpu in computer.Hardware.Where(h => h.HardwareType == HardwareType.Cpu))
                            Collect(cpu, temperatures);
                        double? cpuTemperature = TemperatureSelection.Hottest(temperatures);
                        Emit(output, new(cpuTemperature, null,
                            cpuTemperature.HasValue ? "ready" : "sensor_unavailable",
                            Library + (cpuTemperature.HasValue ? ": hottest CPU core/die sensor" : ": no supported CPU temperature sensor")));
                    }
                }
                catch (Exception error) when (error is not IOException)
                {
                    Emit(output, new(null, null, "failed", Library + ": " + error.GetType().Name + ": " + error.Message[..Math.Min(error.Message.Length, 512)]));
                    // Recreate the hardware objects after an error. Close itself
                    // may block; the helper's independent watchdog bounds it.
                    computer?.Close();
                    computer = null;
                }
                Thread.Sleep(2000);
            }
        }
        catch (IOException) { return 0; } // helper has closed the pipe
        finally { computer?.Close(); }
    }

    private static void Collect(IHardware hardware, List<(string, double?)> values)
    {
        hardware.Update();
        foreach (var sensor in hardware.Sensors)
            if (sensor.SensorType == SensorType.Temperature)
                values.Add((sensor.Name, sensor.Value));
        foreach (var child in hardware.SubHardware) Collect(child, values);
    }

    private static string? CheckPlatformAndDriver()
    {
        // In particular, never run LHM's x86 CPUID code on ARM64, including an
        // x64 process running under Windows ARM64 emulation.
        if (!OperatingSystem.IsWindows() || RuntimeInformation.OSArchitecture != Architecture.X64)
            return "unsupported_platform";
        using var identity = WindowsIdentity.GetCurrent();
        bool admin = new WindowsPrincipal(identity).IsInRole(WindowsBuiltInRole.Administrator);
        if (!admin) return "permission_denied";
        using var registry = RegistryKey.OpenBaseKey(RegistryHive.LocalMachine, RegistryView.Registry64);
        using var key = registry.OpenSubKey(@"SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\PawnIO");
        bool installed = Version.TryParse(key?.GetValue("DisplayVersion") as string, out var version) && version >= new Version(2, 2, 0);
        if (!installed) return "driver_missing";
        using var device = CreateFileW(@"\\?\GLOBALROOT\Device\PawnIO", 3, 3, IntPtr.Zero, 3, 0, IntPtr.Zero);
        return TemperatureSelection.Preflight(true, true, admin, installed, !device.IsInvalid);
    }

    private static string FailureDetail(string status) => status switch
    {
        "unsupported_platform" => "Built-in CPU sensors support Windows x64 Intel/AMD; this architecture requires another sensor provider",
        "permission_denied" => "Built-in CPU sensors require the elevated Tramoing helper",
        "driver_missing" => "PawnIO 2.2 or newer is not installed; repair the Windows installation with CPU temperature selected",
        _ => "PawnIO is installed but its driver cannot be opened; check installation, driver policy or a pending restart"
    };

    private static void Emit(Stream output, Snapshot snapshot)
    {
        byte[] bytes = JsonSerializer.SerializeToUtf8Bytes(snapshot);
        if (bytes.Length > 8192) throw new IOException("Sensor frame too large");
        Span<byte> header = stackalloc byte[4];
        BinaryPrimitives.WriteUInt32LittleEndian(header, (uint)bytes.Length);
        output.Write(header);
        output.Write(bytes);
        output.Flush();
    }

    private sealed record Snapshot(double? cpu, double? gpu, string status, string source);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern SafeFileHandle CreateFileW(string name, uint access, uint share,
        IntPtr security, uint creation, uint flags, IntPtr template);
}
