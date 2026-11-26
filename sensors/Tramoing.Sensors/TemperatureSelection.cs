namespace Tramoing.Sensors;

// Shared by the worker and dependency-free tests. No hardware access here.
internal static class TemperatureSelection
{
    public static double? Hottest(IEnumerable<(string Name, double? Value)> sensors) =>
        sensors.Where(s => !s.Name.Contains("Distance", StringComparison.OrdinalIgnoreCase)
                        && !s.Name.Contains("TjMax", StringComparison.OrdinalIgnoreCase)
                        && s.Value is > 0 and <= 150 && double.IsFinite(s.Value.Value))
            .Select(s => s.Value).DefaultIfEmpty(null).Max();

    // Read adapters independently: a virtual/unsupported GPU must not hide a
    // working physical GPU, and one category must not erase another's reading.
    public static (double? Temperature, int FailedDevices) Read(
        IEnumerable<Func<IEnumerable<(string Name, double? Value)>>> devices)
    {
        var values = new List<(string Name, double? Value)>();
        int failedDevices = 0;
        foreach (var device in devices)
        {
            try
            {
                // Materialize first so a failed read cannot leave partial data.
                values.AddRange(device().ToArray());
            }
            catch (Exception)
            {
                failedDevices++;
            }
        }
        return (Hottest(values), failedDevices);
    }

    public static string Status(double? cpu, double? gpu) =>
        cpu.HasValue || gpu.HasValue ? "ready" : "sensor_unavailable";

    public static string? Preflight(bool windows, bool nativeX64, bool elevated, bool driverInstalled, bool driverOpen) =>
        !windows || !nativeX64 ? "unsupported_platform" :
        !elevated ? "permission_denied" :
        !driverInstalled ? "driver_missing" :
        !driverOpen ? "driver_unavailable" : null;
}
