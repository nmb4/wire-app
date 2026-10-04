# Samples the working set of a process by name, prints peak + timeline CSV.
param(
    [string]$Name = "gpui-atlas-video-bench",
    [int]$Seconds = 25,
    [double]$IntervalMs = 500
)

$peak = 0
$rows = @()
$deadline = (Get-Date).AddSeconds($Seconds)
while ((Get-Date) -lt $deadline) {
    $procs = Get-Process -Name $Name -ErrorAction SilentlyContinue
    foreach ($p in $procs) {
        try { $p.Refresh() } catch {}
        $ws = $p.WorkingSet64
        if ($ws -gt $peak) { $peak = $ws }
        $rows += "$([DateTimeOffset]::Now.ToUnixTimeMilliseconds()),$ws"
    }
    Start-Sleep -Milliseconds $IntervalMs
}
"timestamp_ms,working_set_bytes"
$rows
"PEAK_BYTES=$peak"
"PEAK_MIB={0:N1}" -f ($peak / 1MB)
