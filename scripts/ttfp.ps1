<#
.SYNOPSIS
  Time-to-first-pixel A/B harness for fire.

.DESCRIPTION
  Launches two builds of fire alternately on each test image, N times each, and reports the
  median / mean / standard deviation of the milliseconds from kernel process creation to the
  first image-bearing present. The exe measures itself: with FIRE_TTFP_OUT set it writes that
  number to the named file on its first image frame and exits (see crates/fire/src/ttfp.rs).

  The A/B runs are interleaved with the order flipped on every iteration. The measured spread
  is only ~1.5-2 ms, so "which build happened to run second" would otherwise be a larger effect
  than the thing being measured.

.EXAMPLE
  .\scripts\ttfp.ps1 -A ..\fire-main\target\release\fire.exe -B .\target\release\fire.exe `
      -Images 'D:\img\small.png','D:\img\big.png' -N 12
#>
param(
    [Parameter(Mandatory)] [string] $A,
    [Parameter(Mandatory)] [string] $B,
    [Parameter(Mandatory)] [string[]] $Images,
    [int] $N = 12,
    [string] $LabelA = 'A',
    [string] $LabelB = 'B',
    [string] $Csv = ''
)

$ErrorActionPreference = 'Stop'
$out = Join-Path ([IO.Path]::GetTempPath()) ("fire-ttfp-{0}.txt" -f [Guid]::NewGuid())

function Invoke-Launch([string] $exe, [string] $image) {
    if (Test-Path $out) { Remove-Item $out }
    $env:FIRE_TTFP_OUT = $out
    $p = Start-Process -FilePath $exe -ArgumentList ('"' + $image + '"') -PassThru
    if (-not $p.WaitForExit(30000)) {
        $p.Kill()
        throw "launch of $exe timed out (no image frame within 30 s)"
    }
    if (-not (Test-Path $out)) { throw "$exe exited without writing the stamp" }
    $ms = [double] (Get-Content $out -Raw).Trim()
    Remove-Item $out
    Start-Sleep -Milliseconds 150   # let the previous window's teardown settle
    return $ms
}

$rows = @()
foreach ($image in $Images) {
    $name = Split-Path $image -Leaf
    $size = (Get-Item $image).Length
    Write-Host ("`n== {0} ({1:N0} bytes) ==" -f $name, $size)
    # One warm-up launch per build so the OS file cache holds the exe and the image.
    [void] (Invoke-Launch $A $image)
    [void] (Invoke-Launch $B $image)
    $resA = @(); $resB = @()
    for ($i = 0; $i -lt $N; $i++) {
        if ($i % 2 -eq 0) {
            $resA += Invoke-Launch $A $image
            $resB += Invoke-Launch $B $image
        } else {
            $resB += Invoke-Launch $B $image
            $resA += Invoke-Launch $A $image
        }
        Write-Host ("  {0,2}: {1} {2,7:N1}   {3} {4,7:N1}" -f ($i + 1), $LabelA, $resA[-1], $LabelB, $resB[-1])
        $rows += [pscustomobject]@{ image = $name; iter = $i + 1; build = $LabelA; ms = $resA[-1] }
        $rows += [pscustomobject]@{ image = $name; iter = $i + 1; build = $LabelB; ms = $resB[-1] }
    }
    foreach ($pair in @(@($LabelA, $resA), @($LabelB, $resB))) {
        $label, $r = $pair
        $sorted = $r | Sort-Object
        $median = if ($sorted.Count % 2) { $sorted[[int][Math]::Floor($sorted.Count / 2)] } else { ($sorted[$sorted.Count / 2 - 1] + $sorted[$sorted.Count / 2]) / 2 }
        $mean = ($r | Measure-Object -Average).Average
        $sd = [Math]::Sqrt((($r | ForEach-Object { ($_ - $mean) * ($_ - $mean) }) | Measure-Object -Sum).Sum / [Math]::Max(1, $r.Count - 1))
        Write-Host ("  {0,-8} median {1,7:N1} ms   mean {2,7:N1}   sd {3,5:N1}   min {4,7:N1}   max {5,7:N1}   (n={6})" -f $label, $median, $mean, $sd, $sorted[0], $sorted[-1], $r.Count)
    }
}
if ($Csv) { $rows | Export-Csv -NoTypeInformation -Path $Csv; Write-Host "`nraw results: $Csv" }
