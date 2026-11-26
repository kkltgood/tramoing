using Tramoing.Sensors;

static void Check(bool condition, string message)
{
    if (!condition) throw new Exception(message);
}

Check(TemperatureSelection.Hottest(new (string, double?)[] {
    ("CPU Package", 64), ("Core #1", 71), ("Core #2 Distance to TjMax", 95),
    ("TjMax", 100), ("Missing", null), ("Invalid", double.NaN),
    ("Invalid2", double.PositiveInfinity), ("Invalid3", 65535), ("Zero", 0)
}) == 71, "Intel distance-to-limit and invalid sensors must not mask CPU temperature");
Check(TemperatureSelection.Hottest(new (string, double?)[] {
    ("Core (Tctl/Tdie)", 62.5), ("CCD1 (Tdie)", 67.25)
}) == 67.25, "AMD core/die sensors must remain supported");
Check(TemperatureSelection.Hottest(Array.Empty<(string, double?)>()) == null, "Missing sensor must remain absent");
Check(TemperatureSelection.Preflight(true, false, true, true, true) == "unsupported_platform", "ARM64 must not enter x86 CPU code");
Check(TemperatureSelection.Preflight(true, true, false, true, true) == "permission_denied", "Missing elevation");
Check(TemperatureSelection.Preflight(true, true, true, false, false) == "driver_missing", "Missing driver");
Check(TemperatureSelection.Preflight(true, true, true, true, false) == "driver_unavailable", "Unavailable driver");
Check(TemperatureSelection.Preflight(true, true, true, true, true) == null, "Supported platform");

var cpu = TemperatureSelection.Read(new Func<IEnumerable<(string, double?)>>[] {
    () => new (string, double?)[] { ("CPU Package", 86) }
});
var gpu = TemperatureSelection.Read(new Func<IEnumerable<(string, double?)>>[] {
    () => throw new InvalidOperationException("Unsupported virtual adapter"),
    () => new (string, double?)[] { ("GPU Core", 54), ("GPU Hot Spot", 63), ("Missing", null) },
    () => new (string, double?)[] { ("GPU Core", 59), ("Invalid", -1) }
});
Check(cpu.Temperature == 86 && gpu.Temperature == 63 && gpu.FailedDevices == 1,
    "A failing virtual GPU must not hide CPU or working physical GPU temperatures");

static IEnumerable<(string, double?)> PartialFailure()
{
    yield return ("GPU Core", 70);
    throw new InvalidOperationException("Adapter disappeared");
}
gpu = TemperatureSelection.Read(new Func<IEnumerable<(string, double?)>>[] { PartialFailure });
Check(gpu.Temperature == null && gpu.FailedDevices == 1 && cpu.Temperature == 86,
    "A failed GPU read must discard partial/stale values without clearing the CPU reading");
Check(TemperatureSelection.Status(cpu.Temperature, gpu.Temperature) == "ready", "CPU-only reading remains ready");
Check(TemperatureSelection.Status(null, 54) == "ready", "GPU-only reading must not be discarded by the helper protocol");
Check(TemperatureSelection.Status(null, null) == "sensor_unavailable", "Both absent must remain unavailable");
Console.WriteLine("Sensor selection, failure isolation and platform tests passed (13 checks).");
