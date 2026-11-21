param(
    [Parameter(Mandatory=$true)][string]$Executable,
    [string]$Screenshot
)
$ErrorActionPreference = 'Stop'
$Executable = (Resolve-Path -LiteralPath $Executable).Path
$testRoot = Join-Path $env:TEMP ('tramoing-runtime-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $testRoot | Out-Null
$oldDataDir = $env:TM_DATA_DIR
$oldDbPath = $env:TM_DB_PATH
$env:TM_DATA_DIR = $testRoot
$env:TM_DB_PATH = Join-Path $testRoot 'history.db'

Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class TramoingRuntimeProbe {
 public delegate bool EnumProc(IntPtr h,IntPtr l);
 [StructLayout(LayoutKind.Sequential)] public struct Rect { public int L,T,R,B; }
 [StructLayout(LayoutKind.Sequential)] public struct Point { public int X,Y; }
 [DllImport("user32.dll")] public static extern IntPtr SetThreadDpiAwarenessContext(IntPtr context);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern IntPtr FindWindow(string c,string t);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr p,IntPtr a,string c,string t);
 [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h,out Rect r);
 [DllImport("user32.dll")] public static extern IntPtr GetParent(IntPtr h);
 [DllImport("user32.dll")] public static extern int GetWindowLong(IntPtr h,int n);
 [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h,out uint p);
 [DllImport("user32.dll")] public static extern bool IsWindow(IntPtr h);
 [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h,int command);
 [DllImport("dwmapi.dll")] public static extern int DwmFlush();
 [DllImport("user32.dll")] public static extern IntPtr WindowFromPoint(Point point);
 [DllImport("user32.dll")] public static extern bool GetLayeredWindowAttributes(IntPtr h,out uint key,out byte alpha,out uint flags);
 [DllImport("user32.dll")] public static extern bool EnumWindows(EnumProc p,IntPtr l);
 [DllImport("user32.dll",CharSet=CharSet.Unicode)] public static extern int GetWindowText(IntPtr h,StringBuilder b,int n);
 [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h,uint m,IntPtr w,IntPtr l);
 public static IntPtr Main(uint pid){ IntPtr found=IntPtr.Zero; EnumWindows((h,l)=>{uint p;GetWindowThreadProcessId(h,out p);var b=new StringBuilder(256);GetWindowText(h,b,256);if(p==pid && b.ToString()=="Tramoing"){found=h;return false;}return true;},IntPtr.Zero);return found; }
 public static IntPtr Badge(uint pid){var parent=FindWindow("Shell_TrayWnd",null);var h=IntPtr.Zero;while((h=FindWindowEx(parent,h,"Tramoing.TaskbarSpeed",null))!=IntPtr.Zero){uint p;GetWindowThreadProcessId(h,out p);if(p==pid)return h;}return IntPtr.Zero;}
}
'@
# Only this probe thread: query and screenshot in physical pixels, including 150%/200%.
$previousDpi = [TramoingRuntimeProbe]::SetThreadDpiAwarenessContext([IntPtr]::new(-4))
$testProcess = $null
$badge = [IntPtr]::Zero
try {
    $testProcess = Start-Process -FilePath $Executable -ArgumentList '--mock' -WindowStyle Hidden -PassThru -RedirectStandardError (Join-Path $testRoot 'stderr.log')
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while ($badge -eq [IntPtr]::Zero -and [DateTime]::UtcNow -lt $deadline -and -not $testProcess.HasExited) {
        Start-Sleep -Milliseconds 250
        $badge = [TramoingRuntimeProbe]::Badge($testProcess.Id)
    }
    if ($badge -eq [IntPtr]::Zero) { throw "No taskbar child; logs: $testRoot" }
    if ([TramoingRuntimeProbe]::GetParent($badge) -ne [TramoingRuntimeProbe]::FindWindow('Shell_TrayWnd', $null)) { throw 'Wrong parent' }
    if (([TramoingRuntimeProbe]::GetWindowLong($badge, -16) -band 0x40000000) -eq 0) { throw 'Missing WS_CHILD' }
    if (([TramoingRuntimeProbe]::GetWindowLong($badge, -20) -band 8) -ne 0) { throw 'Unexpected topmost window' }
    [uint32]$colorKey = 0
    [byte]$alpha = 0
    [uint32]$layerFlags = 0
    if (([TramoingRuntimeProbe]::GetWindowLong($badge, -20) -band 0x80000) -eq 0) { throw 'Missing layered child composition' }
    if ([TramoingRuntimeProbe]::GetLayeredWindowAttributes($badge, [ref]$colorKey, [ref]$alpha, [ref]$layerFlags) -and $layerFlags -ne 0) { throw 'Expected per-pixel composition without global alpha/color key' }
    $hitRect = New-Object TramoingRuntimeProbe+Rect
    [TramoingRuntimeProbe]::GetWindowRect($badge, [ref]$hitRect) | Out-Null
    $point = New-Object TramoingRuntimeProbe+Point
    $point.X = $hitRect.L + 3
    $point.Y = $hitRect.T + 3
    if ([TramoingRuntimeProbe]::WindowFromPoint($point) -ne $badge) { throw 'Blank area does not receive native hit testing (or is occluded)' }
    Start-Sleep -Milliseconds 1500
    Add-Type -AssemblyName System.Drawing
    $rect = New-Object TramoingRuntimeProbe+Rect
    [TramoingRuntimeProbe]::GetWindowRect($badge, [ref]$rect) | Out-Null
    $image = New-Object Drawing.Bitmap ($rect.R - $rect.L), ($rect.B - $rect.T)
    $background = New-Object Drawing.Bitmap $image.Width, $image.Height
    $graphics = [Drawing.Graphics]::FromImage($image)
    $backgroundGraphics = [Drawing.Graphics]::FromImage($background)
    try {
        [TramoingRuntimeProbe]::DwmFlush() | Out-Null
        $graphics.CopyFromScreen($rect.L, $rect.T, 0, 0, $image.Size)
        # Briefly hide only the test process's own child to sample the real taskbar.
        [TramoingRuntimeProbe]::ShowWindow($badge, 0) | Out-Null
        [TramoingRuntimeProbe]::DwmFlush() | Out-Null
        $backgroundGraphics.CopyFromScreen($rect.L, $rect.T, 0, 0, $image.Size)
        $backgroundMaxDelta = 0
        foreach ($x in @(3, [int]($image.Width / 2), ($image.Width - 4))) {
            foreach ($y in @(3, ($image.Height - 4))) {
                $actual = $image.GetPixel($x, $y)
                $expected = $background.GetPixel($x, $y)
                foreach ($channel in @('R','G','B')) {
                    $backgroundMaxDelta = [Math]::Max($backgroundMaxDelta, [Math]::Abs([int]$actual.$channel - [int]$expected.$channel))
                }
            }
        }
        if ($backgroundMaxDelta -gt 2) { throw "Background is not transparent; maximum channel delta: $backgroundMaxDelta" }
        if ($Screenshot) { $image.Save([IO.Path]::GetFullPath($Screenshot), [Drawing.Imaging.ImageFormat]::Png) }
    } finally {
        [TramoingRuntimeProbe]::ShowWindow($badge, 8) | Out-Null
        $graphics.Dispose(); $backgroundGraphics.Dispose(); $image.Dispose(); $background.Dispose()
    }
    $second = Start-Process -FilePath $Executable -ArgumentList '--mock' -WindowStyle Hidden -PassThru
    if (-not $second.WaitForExit(5000)) {
        $secondMain = [TramoingRuntimeProbe]::Main($second.Id)
        if ($secondMain -ne [IntPtr]::Zero) { [TramoingRuntimeProbe]::PostMessage($secondMain, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
        throw 'Second instance did not exit'
    }
    if ($second.ExitCode -ne 0) { throw 'Second instance failed' }
    if (Test-Path -LiteralPath $env:TM_DB_PATH) { throw 'Mock wrote a persistent database' }
    [pscustomobject]@{NativeChild=$true;Topmost=$false;PerPixelComposition=$true;BackgroundMaxChannelDelta=$backgroundMaxDelta;BlankAreaHitTest=$true;SingleInstance=$true;MockIsolated=$true;DataDirectory=$testRoot;Screenshot=$Screenshot} | ConvertTo-Json
} finally {
    if ($testProcess) {
        $main = [TramoingRuntimeProbe]::Main($testProcess.Id)
        if ($main -ne [IntPtr]::Zero) { [TramoingRuntimeProbe]::PostMessage($main, 0x10, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null }
        $exited = $testProcess.WaitForExit(10000)
        [pscustomobject]@{CleanExit=$exited;ChildRemoved=(-not [TramoingRuntimeProbe]::IsWindow($badge))} | ConvertTo-Json
    }
    [TramoingRuntimeProbe]::SetThreadDpiAwarenessContext($previousDpi) | Out-Null
    $env:TM_DATA_DIR = $oldDataDir
    $env:TM_DB_PATH = $oldDbPath
}
