namespace Tramoing.Sensors;

// Shared by the worker and dependency-free tests. No hardware access here.
internal static class TemperatureSelection
{
    public static double? Hottest(IEnumerable<(string Name, double? Value)> sensors) =>
        sensors.Where(s => !s.Name.Contains("Distance", StringComparison.OrdinalIgnoreCase)
                        && !s.Name.Contains("TjMax", StringComparison.OrdinalIgnoreCase)
                        && s.Value is > 0 and <= 150 && double.IsFinite(s.Value.Value))
            .Select(s => s.Value).DefaultIfEmpty(null).Max();

    public static string? Preflight(bool windows, bool nativeX64, bool elevated, bool driverInstalled, bool driverOpen) =>
        !windows || !nativeX64 ? "unsupported_platform" :
        !elevated ? "permission_denied" :
        !driverInstalled ? "driver_missing" :
        !driverOpen ? "driver_unavailable" : null;
}
