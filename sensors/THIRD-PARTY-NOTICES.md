# Windows sensor component

Tramoing's sensor adapter is MIT licensed. Third-party components retain their
own licenses; package metadata and license texts are included in `licenses/`.
The managed assemblies remain separate and replaceable.

- LibreHardwareMonitorLib 0.9.6: MPL-2.0. Unmodified upstream package.
  Source: https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/tree/v0.9.6
  Source archive: https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/archive/refs/tags/v0.9.6.zip
- The signed PawnIO modules embedded by that library are provided under LGPL-2.1.
  Their upstream source/release is identified in the library's resources:
  https://github.com/namazso/PawnIO.Modules/releases/tag/0.1.6
  See LibreHardwareMonitor's THIRD-PARTY-NOTICES.txt for the full notices.
- PawnIO 2.2.0: official, unmodified, signed redistributable installer.
  https://github.com/namazso/PawnIO.Setup/releases/tag/2.2.0
  Driver source and GPL-2.0-or-later license with device-IO exception:
  https://github.com/namazso/PawnIO
  The installer includes its own licenses and installs a shared system component.
  Tramoing does not remove that shared driver on uninstall.
- .NET runtime: https://github.com/dotnet/runtime (MIT and third-party notices).
  New workers use the system .NET runtime. Tramoing can download the verified
  official installer when a compatible runtime is missing. Older bundled workers
  retain their private runtime until the component itself is replaced.

The complete dependency versions and integrity hashes are recorded in
`sensors/Tramoing.Sensors/packages.lock.json` in Tramoing's source distribution.
