# Вызывается только с проверенными Rust переменными AddonRoot, Destination и SnapshotOnly.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$sha = [System.Security.Cryptography.SHA256]::Create()
$hashes = [System.Collections.Generic.Dictionary[string,string]]::new([StringComparer]::Ordinal)
$count = 0
$bytes = [long]0
$last = [DateTime]::UtcNow
function Assert-NoLinks([string]$path) {
    $item = Get-Item -LiteralPath $path -Force
    while ($null -ne $item) {
        if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw 'unsafe_path: reparse point в резервной копии аддона'
        }
        if ($item -is [System.IO.FileInfo]) { $item = $item.Directory } else { $item = $item.Parent }
    }
}
function Copy-Tree([System.IO.DirectoryInfo]$directory, [string]$prefix) {
    foreach ($item in $directory.GetFileSystemInfos()) {
        if ($prefix -eq '2055492159/user_files/toolkit' -and $item.Name -eq 'spool') { continue }
        if ($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) {
            throw 'unsafe_path: reparse point в резервной копии аддона'
        }
        $relative = $prefix + '/' + $item.Name
        if ($item -is [System.IO.DirectoryInfo]) {
            if (-not $SnapshotOnly) { [System.IO.Directory]::CreateDirectory((Join-Path $Destination $relative)) | Out-Null }
            Copy-Tree $item $relative
        } else {
            $inputFile = [System.IO.File]::Open($item.FullName, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::Read)
            try {
                if (-not $SnapshotOnly) {
                    $target = Join-Path $Destination $relative
                    $outputFile = [System.IO.File]::Open($target, [System.IO.FileMode]::CreateNew, [System.IO.FileAccess]::Write, [System.IO.FileShare]::None)
                    try { $inputFile.CopyTo($outputFile); $outputFile.Flush($true) } finally { $outputFile.Dispose() }
                    $inputFile.Position = 0
                }
                $digest = [BitConverter]::ToString($sha.ComputeHash($inputFile)).Replace('-', '').ToLowerInvariant()
                $script:bytes += $inputFile.Length
            } finally { $inputFile.Dispose() }
            if (-not $SnapshotOnly) {
                $check = [System.IO.File]::OpenRead($target)
                try { $actual = [BitConverter]::ToString($sha.ComputeHash($check)).Replace('-', '').ToLowerInvariant() } finally { $check.Dispose() }
                if ($actual -ne $digest) { throw 'backup_verification_failed' }
            }
            $hashes[$relative] = $digest
            $script:count += 1
            if (([DateTime]::UtcNow - $script:last).TotalSeconds -ge 2) {
                [Console]::Error.WriteLine("Резервная копия Desktop: проверено файлов — $script:count, байтов — $script:bytes")
                $script:last = [DateTime]::UtcNow
            }
        }
    }
}
try {
    [Console]::Error.WriteLine('Резервная копия Desktop: запущен файловый API Windows')
    if (-not $SnapshotOnly) {
        Assert-NoLinks $Destination
        $a = [System.IO.Path]::GetFullPath($AddonRoot).TrimEnd('\') + '\'
        $b = [System.IO.Path]::GetFullPath($Destination).TrimEnd('\') + '\'
        if ($b.StartsWith($a, [StringComparison]::OrdinalIgnoreCase)) { throw 'Каталог резервной копии должен находиться вне addons21' }
    }
    foreach ($id in @('2055492159', '1788670778')) {
        $source = Join-Path $AddonRoot $id
        if (-not (Test-Path -LiteralPath $source)) { continue }
        Assert-NoLinks $source
        if (-not [System.IO.Directory]::Exists($source)) { throw 'unsafe_path: аддон должен быть каталогом' }
        if (-not $SnapshotOnly) { [System.IO.Directory]::CreateDirectory((Join-Path $Destination $id)) | Out-Null }
        Copy-Tree ([System.IO.DirectoryInfo]::new($source)) $id
    }
    [Console]::Error.WriteLine("Резервная копия Desktop: завершено, файлов — $count, байтов — $bytes")
    ConvertTo-Json -InputObject $hashes -Depth 3 -Compress
} catch {
    [Console]::Error.WriteLine($_.Exception.Message)
    exit 1
} finally { $sha.Dispose() }
