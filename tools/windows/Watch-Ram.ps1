#requires -Version 5.1
<#
.SYNOPSIS
Shows Windows-visible physical RAM using a fresh native snapshot on every tick.
.DESCRIPTION
Runs on Windows 11 in Windows PowerShell 5.1 or PowerShell 7 without elevation.
Reads Windows directly; no TOML configuration, service or telemetry file is needed.
Use Ctrl+C to stop. An in-place, colour dashboard on a blue console shows the
memory table, usage bars, rolling physical RAM history and total-RAM change
events. Redirected output uses the same layout as plain ASCII snapshots.
This observes Windows-visible memory, not the hypervisor's requested allocation.
.EXAMPLE
.\Watch-Ram.ps1
.EXAMPLE
.\Watch-Ram.ps1 -IntervalMilliseconds 250
.EXAMPLE
.\Watch-Ram.ps1 -Samples 5 -AsObject | ConvertTo-Json
.EXAMPLE
.\Watch-Ram.ps1 -AsObject | Export-Csv .\ram-history.csv -NoTypeInformation
#>
[CmdletBinding()]
param(
    [ValidateRange(100, 60000)]
    [int] $IntervalMilliseconds = 1000,
    # Zero runs until cancelled; a positive value provides a bounded observation.
    [ValidateRange(0, 2147483647)]
    [int] $Samples = 0,
    [switch] $AsObject,
    [ValidateRange(10, 3600)]
    [int] $HistorySamples = 60,
    # Useful in transcripts, redirected output, or terminals without cursor support.
    [switch] $NoClear
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
if ($env:OS -ne 'Windows_NT') {
    throw 'This script requires Windows.'
}

if (-not ('RampireTools.RamSnapshot' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
namespace RampireTools {
    public static class RamSnapshot {
        [StructLayout(LayoutKind.Sequential)]
        public struct PerformanceInformation {
            public uint cb;
            public UIntPtr CommitTotal, CommitLimit, CommitPeak;
            public UIntPtr PhysicalTotal, PhysicalAvailable, SystemCache;
            public UIntPtr KernelTotal, KernelPaged, KernelNonpaged, PageSize;
            public uint HandleCount, ProcessCount, ThreadCount;
        }
        [DllImport("psapi.dll", SetLastError = true)]
        [return: MarshalAs(UnmanagedType.Bool)]
        private static extern bool GetPerformanceInfo(
            out PerformanceInformation information, uint size);
        public static PerformanceInformation Read() {
            PerformanceInformation value;
            if (!GetPerformanceInfo(out value,
                    (uint)Marshal.SizeOf(typeof(PerformanceInformation)))) {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "GetPerformanceInfo failed");
            }
            if (value.PageSize.ToUInt64() == 0 || value.PhysicalTotal.ToUInt64() == 0 ||
                    value.PhysicalAvailable.ToUInt64() > value.PhysicalTotal.ToUInt64() ||
                    value.CommitTotal.ToUInt64() > value.CommitLimit.ToUInt64()) {
                throw new InvalidOperationException("Invalid Windows memory snapshot");
            }
            return value;
        }
    }
}
'@
}

# Dashboard geometry: 78 columns by 28 rows fits the 80x30 minimum console.
$FrameWidth = 78
$PanelInner = $FrameWidth - 4
$PlotWidth = 60
$PlotHeight = 5
$Background = [ConsoleColor]::DarkBlue

# Box drawing and bar glyphs for the console; ASCII for redirected snapshots.
$UnicodeGlyphs = @{
    TopLeft = [char] 0x2554; TopRight = [char] 0x2557; BottomLeft = [char] 0x255A
    BottomRight = [char] 0x255D; Horizontal = [char] 0x2550; Vertical = [char] 0x2551
    TeeLeft = [char] 0x255F; TeeRight = [char] 0x2562; Rule = [char] 0x2500
    Full = [char] 0x2588; Empty = [char] 0x2591; TotalMark = [char] 0x2501
    Axis = [char] 0x2524; AxisCorner = [char] 0x2514; Up = [char] 0x25B2; Down = [char] 0x25BC
    Dot = [char] 0x25CF
}
$AsciiGlyphs = @{
    TopLeft = '+'; TopRight = '+'; BottomLeft = '+'; BottomRight = '+'; Horizontal = '='
    Vertical = '|'; TeeLeft = '+'; TeeRight = '+'; Rule = '-'; Full = '#'; Empty = '.'
    TotalMark = 'T'; Axis = '|'; AxisCorner = '+'; Up = '+'; Down = '-'; Dot = '*'
}

function Format-RamValue([decimal] $Bytes) {
    return ('{0:N3} GiB' -f ($Bytes / 1GB))
}

function Get-LoadColor([double] $Percent) {
    if ($Percent -ge 90) { return [ConsoleColor]::Red }
    if ($Percent -ge 70) { return [ConsoleColor]::Yellow }
    return [ConsoleColor]::Green
}

# A frame is a list of lines; each line is a list of coloured text segments.
# Adjacent segments with the same colours merge so rendering stays cheap.
function Start-FrameLine {
    $script:FrameLine = New-Object 'System.Collections.Generic.List[object]'
    $script:Frame.Add($script:FrameLine)
}

function Add-Text([string] $Text, [ConsoleColor] $Foreground = [ConsoleColor]::Gray,
        [ConsoleColor] $Back = $Background) {
    if ($Text.Length -eq 0) { return }
    $last = if ($script:FrameLine.Count -gt 0) { $script:FrameLine[$script:FrameLine.Count - 1] }
    if ($null -ne $last -and $last.Foreground -eq $Foreground -and $last.Back -eq $Back) {
        $last.Text += $Text
    } else {
        $script:FrameLine.Add(@{ Text = $Text; Foreground = $Foreground; Back = $Back })
    }
}

function Get-LineLength {
    $length = 0
    foreach ($segment in $script:FrameLine) { $length += $segment.Text.Length }
    return $length
}

# Pads the current line to $Column, keeping the colours of the padding.
function Add-PadTo([int] $Column, [ConsoleColor] $Back = $Background) {
    $missing = $Column - (Get-LineLength)
    if ($missing -gt 0) { Add-Text (' ' * $missing) ([ConsoleColor]::Gray) $Back }
}

function Add-Bar([string] $Title, [ConsoleColor] $Fill, [string] $Text) {
    Start-FrameLine
    $label = " $Title "
    Add-Text $label ([ConsoleColor]::White) $Fill
    Add-Text (' ' * ($FrameWidth - $label.Length - $Text.Length)) ([ConsoleColor]::White) $Fill
}

function Add-PanelTop([string] $Title, [string] $Detail = '') {
    Start-FrameLine
    Add-Text ([string] $G.TopLeft + $G.Horizontal) ([ConsoleColor]::Cyan)
    Add-Text " $Title " ([ConsoleColor]::White)
    if ($Detail) { Add-Text "$Detail " ([ConsoleColor]::DarkCyan) }
    Add-Text ([string] $G.Horizontal * ($FrameWidth - 1 - (Get-LineLength))) ([ConsoleColor]::Cyan)
    Add-Text ([string] $G.TopRight) ([ConsoleColor]::Cyan)
}

function Start-PanelRow {
    Start-FrameLine
    Add-Text ([string] $G.Vertical + ' ') ([ConsoleColor]::Cyan)
}

function Stop-PanelRow {
    Add-PadTo ($FrameWidth - 2)
    Add-Text (' ' + [string] $G.Vertical) ([ConsoleColor]::Cyan)
}

function Add-PanelRule {
    Start-FrameLine
    Add-Text ([string] $G.TeeLeft + ([string] $G.Rule * ($FrameWidth - 2)) + $G.TeeRight) ([ConsoleColor]::Cyan)
}

function Add-PanelBottom {
    Start-FrameLine
    Add-Text ([string] $G.BottomLeft + ([string] $G.Horizontal * ($FrameWidth - 2)) + $G.BottomRight) ([ConsoleColor]::Cyan)
}

function Add-Meter([string] $Label, [double] $Percent, [string] $Detail) {
    $width = 36
    $filled = [int] [math]::Round([math]::Max(0, [math]::Min(100, $Percent)) * $width / 100)
    Start-PanelRow
    Add-Text ($Label.PadRight(10)) ([ConsoleColor]::Gray)
    Add-Text ([string] $G.Full * $filled) (Get-LoadColor $Percent)
    Add-Text ([string] $G.Empty * ($width - $filled)) ([ConsoleColor]::DarkCyan)
    Add-Text (' {0,5:N1}%' -f $Percent) (Get-LoadColor $Percent)
    Add-Text ("  $Detail") ([ConsoleColor]::Gray)
    Stop-PanelRow
}

function Add-MetricRow([string] $Label, [string] $Now, [string] $Session,
        [ConsoleColor] $NowColor = [ConsoleColor]::White) {
    Start-PanelRow
    Add-Text ($Label.PadRight(20)) ([ConsoleColor]::Gray)
    Add-Text ($Now.PadLeft(16)) $NowColor
    Add-Text '    ' ([ConsoleColor]::Gray)
    Add-Text $Session ([ConsoleColor]::Yellow)
    Stop-PanelRow
}

$history = New-Object 'System.Collections.Generic.List[object]'
$events = New-Object 'System.Collections.Generic.List[object]'
$previousTotal = $null
$baselineTotal = $null
$minimumTotal = $null
$maximumTotal = $null
$minimumAvailable = $null
$peakInUse = [uint64] 0
$addedBytes = [decimal] 0
$removedBytes = [decimal] 0
$sampleNumber = 0
$startedAt = [DateTimeOffset]::Now
$interactive = $false
$oldCursorVisible = $true
$oldForeground = $null
$oldBackground = $null
$oldEncoding = $null
if (-not $AsObject -and -not $NoClear) {
    try {
        $interactive = -not [Console]::IsOutputRedirected -and
            [Console]::WindowWidth -ge 80 -and [Console]::WindowHeight -ge 30
        if ($interactive) {
            $oldCursorVisible = [Console]::CursorVisible
            $oldForeground = [Console]::ForegroundColor
            $oldBackground = [Console]::BackgroundColor
            $oldEncoding = [Console]::OutputEncoding
            [Console]::OutputEncoding = [Text.Encoding]::UTF8
            [Console]::BackgroundColor = $Background
            [Console]::Clear()
            [Console]::CursorVisible = $false
        }
    } catch {
        # ISE, remoting and redirected hosts may not expose a console.
        $interactive = $false
    }
}

function Restore-Console {
    if ($null -ne $oldEncoding) { [Console]::OutputEncoding = $oldEncoding }
    if ($null -ne $oldBackground) { [Console]::BackgroundColor = $oldBackground }
    if ($null -ne $oldForeground) { [Console]::ForegroundColor = $oldForeground }
    [Console]::CursorVisible = $oldCursorVisible
}

try {
    while ($Samples -eq 0 -or $sampleNumber -lt $Samples) {
        # Never cache physical total: hot-add/removal can change it between ticks.
        $native = [RampireTools.RamSnapshot]::Read()
        $pageSize = [decimal] $native.PageSize.ToUInt64()
        $total = [uint64] ([decimal] $native.PhysicalTotal.ToUInt64() * $pageSize)
        $available = [uint64] ([decimal] $native.PhysicalAvailable.ToUInt64() * $pageSize)
        $commit = [uint64] ([decimal] $native.CommitTotal.ToUInt64() * $pageSize)
        $limit = [uint64] ([decimal] $native.CommitLimit.ToUInt64() * $pageSize)
        $inUse = $total - $available
        $now = [DateTimeOffset]::Now
        # Decimal subtraction preserves negative deltas without unsigned underflow.
        $delta = if ($null -eq $previousTotal) { [decimal] 0 } else {
            [decimal] $total - [decimal] $previousTotal
        }
        if ($null -eq $baselineTotal) {
            $baselineTotal = $total
            $minimumTotal = $total
            $maximumTotal = $total
            $minimumAvailable = $available
        }
        $minimumTotal = [math]::Min($minimumTotal, $total)
        $maximumTotal = [math]::Max($maximumTotal, $total)
        $minimumAvailable = [math]::Min($minimumAvailable, $available)
        $peakInUse = [math]::Max($peakInUse, $inUse)
        if ($delta -ne 0) {
            if ($delta -gt 0) { $addedBytes += $delta } else { $removedBytes -= $delta }
            $events.Insert(0, [pscustomobject] @{ Timestamp = $now; Delta = $delta; Total = $total })
            if ($events.Count -gt 3) { $events.RemoveAt(3) }
        }
        $row = [pscustomobject] [ordered] @{
            Timestamp = $now
            TotalBytes = $total
            AvailableBytes = $available
            InUseBytes = $inUse
            CommitBytes = $commit
            CommitLimitBytes = $limit
            CommitHeadroomBytes = $limit - $commit
            TotalDeltaBytes = $delta
            TotalGiB = [math]::Round($total / 1GB, 3)
            AvailableGiB = [math]::Round($available / 1GB, 3)
            SessionMinimumTotalBytes = $minimumTotal
            SessionMaximumTotalBytes = $maximumTotal
            SessionMinimumAvailableBytes = $minimumAvailable
            SessionPeakInUseBytes = $peakInUse
            SessionAddedBytes = $addedBytes
            SessionRemovedBytes = $removedBytes
        }
        if ($AsObject) {
            $row
        } else {
            $history.Add($row)
            if ($history.Count -gt $HistorySamples) { $history.RemoveAt(0) }
            $G = if ($interactive) { $UnicodeGlyphs } else { $AsciiGlyphs }
            $script:Frame = New-Object 'System.Collections.Generic.List[object]'
            $usedPercent = 100.0 * $inUse / $total
            $commitPercent = 100.0 * $commit / $limit

            $clock = '{0:HH:mm:ss}' -f $now
            Add-Bar ('{0} VIRTIO-MEM RAM MONITOR  {1}' -f $G.Dot, $env:COMPUTERNAME) ([ConsoleColor]::DarkCyan) $clock
            Add-Text $clock ([ConsoleColor]::White) ([ConsoleColor]::DarkCyan)
            Start-FrameLine
            Add-Text (' Session {0}   samples {1}   refresh {2} ms' -f
                ($now - $startedAt).ToString('hh\:mm\:ss'), ($sampleNumber + 1), $IntervalMilliseconds) ([ConsoleColor]::Gray)
            Add-PadTo $FrameWidth

            Add-PanelTop 'PHYSICAL MEMORY' 'now / session'
            Add-MetricRow 'Total' (Format-RamValue $total) ('min {0:N3} / max {1:N3} GiB' -f
                ($minimumTotal / 1GB), ($maximumTotal / 1GB))
            Add-MetricRow 'In use' (Format-RamValue $inUse) ('peak {0:N3} GiB' -f ($peakInUse / 1GB)) (Get-LoadColor $usedPercent)
            Add-MetricRow 'Available' (Format-RamValue $available) ('low {0:N3} GiB' -f ($minimumAvailable / 1GB))
            $net = [decimal] $total - $baselineTotal
            $netColor = if ($net -gt 0) { [ConsoleColor]::Green } elseif ($net -lt 0) { [ConsoleColor]::Red } else { [ConsoleColor]::White }
            Add-MetricRow 'Net total change' ('{0:+0.00;-0.00;0.00} MiB' -f ($net / 1MB)) ('added {0:N1} / removed {1:N1} MiB' -f
                ($addedBytes / 1MB), ($removedBytes / 1MB)) $netColor
            Add-MetricRow 'Commit headroom' (Format-RamValue ($limit - $commit)) ('{0} of {1}' -f
                (Format-RamValue $commit), (Format-RamValue $limit))
            Add-PanelRule
            Add-Meter 'In use' $usedPercent ('{0:N2} / {1:N2} GiB' -f ($inUse / 1GB), ($total / 1GB))
            Add-Meter 'Commit' $commitPercent ('{0:N2} / {1:N2} GiB' -f ($commit / 1GB), ($limit / 1GB))
            Add-PanelBottom

            # Absolute GiB, not percentages: total changes remain visible even
            # when Windows grows memory during a workload. Scale includes total.
            $scale = [math]::Ceiling((($history | Measure-Object TotalBytes -Maximum).Maximum / 1GB))
            $scale = [math]::Max(1, $scale)
            $points = $history.ToArray()
            if ($history.Count -gt $PlotWidth) {
                # Compress the entire selected window into columns, preserving
                # peak in-use within each bin and its latest physical total.
                $points = @(for ($column = 0; $column -lt $PlotWidth; $column++) {
                    $first = [int] [math]::Floor($column * $history.Count / $PlotWidth)
                    $last = [int] [math]::Floor(($column + 1) * $history.Count / $PlotWidth) - 1
                    $peak = [uint64] 0
                    for ($index = $first; $index -le $last; $index++) {
                        $peak = [math]::Max($peak, $history[$index].InUseBytes)
                    }
                    [pscustomobject] @{
                        Timestamp = $history[$last].Timestamp
                        TotalBytes = $history[$last].TotalBytes
                        InUseBytes = $peak
                    }
                })
            }
            $span = ($history[$history.Count - 1].Timestamp - $history[0].Timestamp).TotalSeconds
            Add-PanelTop 'RAM HISTORY' ('last {0:N0}s  {1}=total  {2}=peak used  0..{3} GiB' -f
                $span, $G.TotalMark, $G.Full, $scale)
            for ($y = $PlotHeight; $y -ge 0; $y--) {
                Start-PanelRow
                Add-Text ('{0,5:N1} {1}' -f ($scale * $y / $PlotHeight), $G.Axis) ([ConsoleColor]::DarkCyan)
                foreach ($point in $points) {
                    $totalLevel = [int] [math]::Round(($point.TotalBytes / 1GB) / $scale * $PlotHeight)
                    $usedLevel = [int] [math]::Round(($point.InUseBytes / 1GB) / $scale * $PlotHeight)
                    if ($y -eq $totalLevel) {
                        Add-Text ([string] $G.TotalMark) ([ConsoleColor]::Yellow)
                    } elseif ($y -le $usedLevel) {
                        Add-Text ([string] $G.Full) (Get-LoadColor (100.0 * $point.InUseBytes / $point.TotalBytes))
                    } else {
                        Add-Text ' '
                    }
                }
                Stop-PanelRow
            }
            Start-PanelRow
            Add-Text ('      ' + [string] $G.AxisCorner + ([string] $G.Rule * $PlotWidth)) ([ConsoleColor]::DarkCyan)
            Stop-PanelRow
            Start-PanelRow
            Add-Text ('       {0:HH:mm:ss} oldest' -f $points[0].Timestamp) ([ConsoleColor]::Gray)
            Add-PadTo (2 + 7 + $PlotWidth - 15)
            Add-Text ('newest {0:HH:mm:ss}' -f $points[-1].Timestamp) ([ConsoleColor]::Gray)
            Stop-PanelRow
            Add-PanelBottom

            Add-PanelTop 'TOTAL RAM CHANGES' 'newest first'
            for ($eventIndex = 0; $eventIndex -lt 3; $eventIndex++) {
                Start-PanelRow
                if ($eventIndex -lt $events.Count) {
                    $change = $events[$eventIndex]
                    $up = $change.Delta -gt 0
                    $color = if ($up) { [ConsoleColor]::Green } else { [ConsoleColor]::Red }
                    Add-Text ('{0:HH:mm:ss}  ' -f $change.Timestamp) ([ConsoleColor]::Gray)
                    Add-Text ('{0} {1,10:+0.00;-0.00} MiB' -f $(if ($up) { $G.Up } else { $G.Down }), ($change.Delta / 1MB)) $color
                    Add-Text ('   total {0:N3} GiB' -f ($change.Total / 1GB)) ([ConsoleColor]::White)
                } elseif ($eventIndex -eq 0) {
                    Add-Text 'No total-RAM changes observed this session.' ([ConsoleColor]::DarkCyan)
                }
                Stop-PanelRow
            }
            Add-PanelBottom
            Add-Bar 'Ctrl+C stop | Windows-visible RAM, not hypervisor requested bytes' ([ConsoleColor]::DarkCyan) ''

            $plain = @($script:Frame | ForEach-Object {
                (@($_ | ForEach-Object { $_.Text }) -join '').TrimEnd()
            }) -join [Environment]::NewLine
            if ($interactive) {
                try {
                    if ([Console]::WindowWidth -le $FrameWidth -or [Console]::WindowHeight -le $script:Frame.Count) {
                        throw 'Console resized below dashboard dimensions.'
                    }
                    [Console]::SetCursorPosition(0, 0)
                    foreach ($line in $script:Frame) {
                        foreach ($segment in $line) {
                            [Console]::ForegroundColor = $segment.Foreground
                            [Console]::BackgroundColor = $segment.Back
                            [Console]::Write($segment.Text)
                        }
                        [Console]::BackgroundColor = $Background
                        [Console]::Write([Environment]::NewLine)
                    }
                } catch {
                    # Continue collecting if the terminal is resized or disconnected.
                    Restore-Console
                    $interactive = $false
                    Write-Host $plain
                }
            } else {
                Write-Host $plain
            }
        }
        $previousTotal = $total
        $sampleNumber++
        if ($Samples -eq 0 -or $sampleNumber -lt $Samples) {
            Start-Sleep -Milliseconds $IntervalMilliseconds
        }
    }
} finally {
    if ($interactive) {
        Restore-Console
        [Console]::WriteLine()
    }
}
