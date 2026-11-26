using System.Buffers.Binary;
using System.Runtime.InteropServices;
using System.Security.Principal;
using System.Text.Json;
using LibreHardwareMonitor.Hardware;
using Microsoft.Win32;
using Microsoft.Win32.SafeHandles;
using Tramoing.Sensors;

// The elevated helper launches the sampling mode. --probe only reports the
// installed component/runtime without touching hardware or requiring elevation.
// No command line paths, plugins, fan controls or driver commands are exposed.
internal static class Program
{
    private const string Library = "LibreHardwareMonitor 0.9.6 / PawnIO";

    private static int Main(string[] args)
    {
        if (args is ["--probe"])
        {
            var version = typeof(Program).Assembly.GetName().Version!;
            Console.WriteLine(JsonSerializer.Serialize(new
            {
                protocol = 1,
                component_version = $"{version.Major}.{version.Minor}.{version.Build}",
                architecture = RuntimeInformation.ProcessArchitecture.ToString(),
                os_architecture = RuntimeInformation.OSArchitecture.ToString(),
                runtime = Environment.Version.ToString()
            }));
            return 0;
        }
        if (args.Length != 0) return 2;
        return Sample();
    }

    // Keep hardware types outside Main so --probe never initializes the
    // hardware library and can run as a normal user before installing PawnIO.
    private static int Sample()
    {
        // The helper assigns our process to a kill-on-close job before sending
        // this handshake. Do not open any sensor hardware before then.
        if (Console.ReadLine() != "start") return 2;
        var output = Console.OpenStandardOutput();
        Console.SetOut(TextWriter.Null); // library diagnostics cannot corrupt the binary stream
        Computer? computer = null;
        long gpuRetryAt = 0;
        string? gpuOpenError = null;
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
                        // LHM's GPU setter may add some vendor groups before
                        // throwing without updating IsGpuEnabled. Recreate the
                        // computer to retry; toggling that flag would leak groups.
                        if (computer != null && gpuOpenError != null && Environment.TickCount64 >= gpuRetryAt)
                        {
                            computer.Close();
                            computer = null;
                        }
                        if (computer == null)
                        {
                            Emit(output, new(null, null, "starting", Library + ": opening CPU/GPU sensors"));
                            computer = new Computer { IsCpuEnabled = true };
                            computer.Open();
                            gpuOpenError = null;
                            // Open GPUs after CPU setup so a vendor driver exception
                            // cannot discard working CPU sensors. LHM uses NVAPI/ADL
                            // here, providing temperatures even without WDDM telemetry.
                            try
                            {
                                computer.IsGpuEnabled = true;
                            }
                            catch (Exception error)
                            {
                                gpuRetryAt = Environment.TickCount64 + 60_000;
                                gpuOpenError = "GPU initialization failed: " + error.GetType().Name;
                            }
                        }
                        var cpu = ReadHardware(computer, h => h.HardwareType == HardwareType.Cpu);
                        var gpu = ReadHardware(computer, h => h.HardwareType is
                            HardwareType.GpuNvidia or HardwareType.GpuAmd or HardwareType.GpuIntel);
                        string detail = Library + ": " + Describe("CPU core/die", cpu)
                            + "; " + Describe("GPU", gpu);
                        if (gpuOpenError != null) detail += "; " + gpuOpenError;
                        Emit(output, new(cpu.Temperature, gpu.Temperature,
                            TemperatureSelection.Status(cpu.Temperature, gpu.Temperature), detail));
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

    private static (double? Temperature, int FailedDevices) ReadHardware(
        Computer computer, Func<IHardware, bool> include) =>
        TemperatureSelection.Read(computer.Hardware.Where(include).Select(hardware =>
            (Func<IEnumerable<(string Name, double? Value)>>)(() =>
            {
                var values = new List<(string, double?)>();
                Collect(hardware, values);
                return values;
            })));

    private static string Describe(string category, (double? Temperature, int FailedDevices) reading) =>
        (reading.Temperature.HasValue ? "hottest " + category + " sensor" : "no supported " + category + " temperature sensor")
        + (reading.FailedDevices > 0 ? $" ({reading.FailedDevices} device reads failed)" : "");

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
        "driver_missing" => "PawnIO 2.2 or newer is not installed; install the temperature component from Settings",
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
