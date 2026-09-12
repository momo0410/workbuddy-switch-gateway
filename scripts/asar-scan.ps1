# 在 app.asar 中按 ASCII needle 扫描并打印上下文（只读，不修改目标文件）。
# 用法: pwsh -File asar-scan.ps1 -Path <asar> -Needle workbuddy-ai -Before 200 -After 400 -Max 30
param(
  [Parameter(Mandatory = $true)][string]$Path,
  [Parameter(Mandatory = $true)][string[]]$Needle,
  [int]$Before = 200,
  [int]$After = 400,
  [int]$Max = 20
)

$enc = [System.Text.Encoding]::UTF8
$pats = @{}
foreach ($n in $Needle) { $pats[$n] = [System.Text.Encoding]::ASCII.GetBytes($n) }

$fs = [System.IO.File]::OpenRead($Path)
$bufSize = 8MB
$buf = New-Object byte[] $bufSize
$overlap = 1024
$counts = @{}
foreach ($n in $Needle) { $counts[$n] = 0 }
$pos = 0
$prev = New-Object byte[] 0

function Show([byte[]]$chunk, [int]$idx, [int]$patLen, [string]$name, [long]$base) {
  $start = [Math]::Max(0, $idx - $Before)
  $end = [Math]::Min($chunk.Length - 1, $idx + $patLen + $After)
  $len = $end - $start + 1
  if ($len -le 0) { return }
  $seg = New-Object byte[] $len
  [Array]::Copy($chunk, $start, $seg, 0, $len)
  $text = $enc.GetString($seg) -replace '[^\x20-\x7E\r\n]', '.'
  "########## [$name] offset $($base + $idx)"
  $text
  ""
}

while (($read = $fs.Read($buf, 0, $bufSize)) -gt 0) {
  $chunk = New-Object byte[] ($prev.Length + $read)
  [Array]::Copy($prev, 0, $chunk, 0, $prev.Length)
  [Array]::Copy($buf, 0, $chunk, $prev.Length, $read)
  $base = $pos - $prev.Length

  foreach ($name in $pats.Keys) {
    $pat = $pats[$name]
    $idx = 0
    while ($true) {
      $idx = [System.Array]::IndexOf($chunk, $pat[0], $idx)
      if ($idx -lt 0) { break }
      if ($idx + $pat.Length -le $chunk.Length) {
        $ok = $true
        for ($k = 1; $k -lt $pat.Length; $k++) {
          if ($chunk[$idx + $k] -ne $pat[$k]) { $ok = $false; break }
        }
        if ($ok) {
          $counts[$name]++
          if ($counts[$name] -le $Max) { Show $chunk $idx $pat.Length $name $base }
        }
      }
      $idx++
    }
  }

  $keep = [Math]::Min($overlap, $chunk.Length)
  $prev = New-Object byte[] $keep
  [Array]::Copy($chunk, $chunk.Length - $keep, $prev, 0, $keep)
  $pos += $read
}
$fs.Close()

"===== counts ====="
$counts.GetEnumerator() | Sort-Object Name | ForEach-Object { "$($_.Name) = $($_.Value)" }
