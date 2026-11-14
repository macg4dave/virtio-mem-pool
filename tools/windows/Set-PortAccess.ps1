# Give LocalService read/write access to one named virtio-serial port, and
# nothing else. Run elevated while RampireGuest is stopped. The QEMU
# guest-agent port is never modified.
function Set-RampirePortAccess {
    param(
        [Parameter(Mandatory=$true)][string] $PortName,
        [Parameter(Mandatory=$true)][string] $BackupDirectory
    )
    if (-not ('RampirePortAccess' -as [type])) {
        Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;
public static class RampirePortAccess {
    [StructLayout(LayoutKind.Sequential)] struct DeviceInfo {
        public uint Size; public Guid ClassGuid; public uint DevInst; public UIntPtr Reserved;
    }
    [DllImport("kernel32.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern uint QueryDosDevice(string name, StringBuilder target, int length);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern IntPtr SetupDiGetClassDevs(IntPtr guid, string enumerator, IntPtr window, uint flags);
    [DllImport("setupapi.dll", SetLastError=true)]
    static extern bool SetupDiEnumDeviceInfo(IntPtr set, uint index, ref DeviceInfo info);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool SetupDiGetDeviceRegistryProperty(IntPtr set, ref DeviceInfo info, uint property, out uint type, byte[] data, uint size, out uint needed);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool SetupDiSetDeviceRegistryProperty(IntPtr set, ref DeviceInfo info, uint property, byte[] data, uint size);
    [DllImport("setupapi.dll", CharSet=CharSet.Unicode, SetLastError=true)]
    static extern bool SetupDiGetDeviceInstanceId(IntPtr set, ref DeviceInfo info, StringBuilder id, uint size, out uint needed);
    [DllImport("setupapi.dll")] static extern bool SetupDiDestroyDeviceInfoList(IntPtr set);
    static string Property(IntPtr set, ref DeviceInfo info, uint property) {
        byte[] bytes = new byte[8192]; uint type, needed;
        if (!SetupDiGetDeviceRegistryProperty(set, ref info, property, out type, bytes, (uint)bytes.Length, out needed)) {
            int error = Marshal.GetLastWin32Error();
            if (error == 13 || error == 2) return null;
            throw new Win32Exception(error);
        }
        return Encoding.Unicode.GetString(bytes, 0, checked((int)needed)).TrimEnd('\0');
    }
    // Returns the exact device instance, previous descriptor and whether a
    // restart is needed. A missing port is a supported fallback.
    public static string[] Configure(string port, bool apply, string expectedInstance) {
        var target = new StringBuilder(2048);
        if (QueryDosDevice(port, target, target.Capacity) == 0) {
            int error = Marshal.GetLastWin32Error();
            if (error == 2) return null;
            throw new Win32Exception(error);
        }
        IntPtr set = SetupDiGetClassDevs(IntPtr.Zero, null, IntPtr.Zero, 6);
        if (set == new IntPtr(-1)) throw new Win32Exception();
        try {
            for (uint index=0;;index++) {
                DeviceInfo info = new DeviceInfo(); info.Size = (uint)Marshal.SizeOf(typeof(DeviceInfo));
                if (!SetupDiEnumDeviceInfo(set, index, ref info)) {
                    int error = Marshal.GetLastWin32Error();
                    if (error == 259) break;
                    throw new Win32Exception(error);
                }
                if (!String.Equals(Property(set, ref info, 14), target.ToString(), StringComparison.OrdinalIgnoreCase)) continue;
                string previous = Property(set, ref info, 24);
                string desired = "D:P(A;;GA;;;SY)(A;;GA;;;BA)(A;;GRGW;;;LS)";
                var id = new StringBuilder(2048); uint needed;
                if (!SetupDiGetDeviceInstanceId(set, ref info, id, (uint)id.Capacity, out needed)) throw new Win32Exception();
                if (previous == desired) return new [] {id.ToString(), previous, "unchanged"};
                if (!apply) return new [] {id.ToString(), previous, "changed"};
                if (!String.Equals(expectedInstance, id.ToString(), StringComparison.OrdinalIgnoreCase)) throw new InvalidOperationException("port device changed during provisioning");
                byte[] bytes = Encoding.Unicode.GetBytes(desired + "\0");
                if (!SetupDiSetDeviceRegistryProperty(set, ref info, 24, bytes, (uint)bytes.Length)) throw new Win32Exception();
                return new [] {id.ToString(), previous, "changed"};
            }
            throw new InvalidOperationException("named port has no matching device instance");
        } finally { SetupDiDestroyDeviceInfoList(set); }
    }
}
'@
    }
    $result = [RampirePortAccess]::Configure($PortName, $false, $null)
    if ($null -eq $result) { return 'absent; control files fallback' }
    [ordered]@{port=$PortName; instance_id=$result[0]; previous_security=$result[1]; action=$result[2]} |
        ConvertTo-Json | Set-Content -LiteralPath (Join-Path $BackupDirectory ($PortName + '-security.json'))
    if ($result[2] -eq 'changed') {
        $applied = [RampirePortAccess]::Configure($PortName, $true, $result[0])
        if ($null -eq $applied) { throw 'port device disappeared during provisioning' }
        & pnputil.exe /restart-device $result[0] | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "port device restart failed: $LASTEXITCODE" }
    }
    return $result[2]
}
