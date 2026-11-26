<#
.SYNOPSIS
Summarizes consecutive host Present intervals and the wall time covered by mapping batches.
.DESCRIPTION
Reads relative ticks from a victory-present-window-v1 JSON file and writes a JSON receipt.
P50 is the sample median. P95 and P99 use the nearest-rank convention.
Overlap measures simultaneity. It can include GPU execution and independent CPU work.
#>
param(
    [string]$Path = (Join-Path $PSScriptRoot '../docs/benchmarks/present-window.json')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$window = Get-Content -LiteralPath $Path -Raw | ConvertFrom-Json
if ($window.schema -ne 'victory-present-window-v1') {
    throw 'Unrecognized Present window schema.'
}

function Test-JsonInteger($Value) {
    return ($Value -is [int] -or $Value -is [long])
}

$hz = $window.qpc_hz
$count = $window.intervals.Count
if (!(Test-JsonInteger $hz) -or $hz -le 0 -or !(Test-JsonInteger $window.interval_count) -or
    $count -le 0 -or $window.interval_count -ne $count) {
    throw 'The QPC frequency and declared interval count must be positive integers and the count must match the data.'
}

$durations = [System.Collections.Generic.List[long]]::new()
$elapsed = 0L
$overlap = 0L
$overBudget = 0
$withoutMapper = 0
$overBudgetWithoutMapper = 0
foreach ($row in $window.intervals) {
    $ticks = $row.duration_ticks
    $covered = $row.mapper_overlap_ticks
    if (!(Test-JsonInteger $ticks) -or !(Test-JsonInteger $covered) -or
        $ticks -le 0 -or $covered -lt 0 -or $covered -gt $ticks) {
        throw "Invalid duration or overlap at interval $($durations.Count + 1)."
    }
    $durations.Add($ticks)
    $elapsed += $ticks
    $overlap += $covered
    $late = $ticks -gt ($hz / 30.0)
    if ($late) { $overBudget++ }
    if ($covered -eq 0) {
        $withoutMapper++
        if ($late) { $overBudgetWithoutMapper++ }
    }
}

$durations.Sort()
$msPerTick = 1000.0 / $hz
$left = [int][Math]::Floor(($count - 1) / 2.0)
$right = [int][Math]::Floor($count / 2.0)
$p95 = [int][Math]::Ceiling(0.95 * $count) - 1
$p99 = [int][Math]::Ceiling(0.99 * $count) - 1
$meanMs = $elapsed * $msPerTick / $count
[ordered]@{
    present_intervals = $count
    elapsed_seconds = $elapsed / [double]$hz
    observed_fps = $count * [double]$hz / $elapsed
    mean_ms = $meanMs
    p50_ms = ($durations[$left] + $durations[$right]) * $msPerTick / 2.0
    p95_ms = $durations[$p95] * $msPerTick
    p99_ms = $durations[$p99] * $msPerTick
    max_ms = $durations[$count - 1] * $msPerTick
    intervals_over_30fps_budget = $overBudget
    intervals_without_mapper_overlap = $withoutMapper
    intervals_over_budget_without_mapper_overlap = $overBudgetWithoutMapper
    mapper_overlap_total_ms = $overlap * $msPerTick
    mapper_overlap_mean_ms = $overlap * $msPerTick / $count
    mean_ms_to_save_for_30fps = [Math]::Max(0.0, $meanMs - 1000.0 / 30)
} | ConvertTo-Json
