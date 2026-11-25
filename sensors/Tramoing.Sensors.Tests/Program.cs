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
Console.WriteLine("Sensor selection and platform tests passed (8 checks).");
