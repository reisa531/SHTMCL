$ErrorActionPreference = 'Stop'

$repoRoot = Split-Path -Parent $PSScriptRoot
$releaseExe = Join-Path $repoRoot 'target/release/SHTMCL.exe'
$portableDir = Join-Path $repoRoot 'target/portable'
$portableExe = Join-Path $portableDir 'SHTMCL.exe'

$hostTriple = (& rustc -vV | Where-Object { $_ -like 'host: *' } | ForEach-Object { $_.Substring(6) })
if ($hostTriple -ne 'x86_64-pc-windows-msvc') {
    throw "This script requires the x86_64-pc-windows-msvc toolchain. Current host: $hostTriple"
}

Push-Location $repoRoot
try {
    & npm run build --workspace @shtmcl/desktop
    if ($LASTEXITCODE -ne 0) { throw "Frontend build failed with exit code $LASTEXITCODE" }

    & cargo build --locked --offline --release -p shtmcl-desktop --bin SHTMCL --features custom-protocol
    if ($LASTEXITCODE -ne 0) { throw "Windows release build failed with exit code $LASTEXITCODE" }

    if (!(Test-Path -LiteralPath $releaseExe -PathType Leaf)) {
        throw "Release executable was not created: $releaseExe"
    }

    $item = Get-Item -LiteralPath $releaseExe
    $hardLimitBytes = 40 * 1MB
    $targetBytes = 25 * 1MB
    if ($item.Length -gt $hardLimitBytes) {
        throw "SHTMCL.exe is $($item.Length) bytes; the 40 MiB release limit is $hardLimitBytes bytes."
    }
    if ($item.Length -gt $targetBytes) {
        Write-Warning "SHTMCL.exe is $($item.Length) bytes, above the 25 MiB target."
    }

    New-Item -ItemType Directory -Force -Path $portableDir | Out-Null
    Copy-Item -LiteralPath $releaseExe -Destination $portableExe -Force
    $hash = (Get-FileHash -LiteralPath $portableExe -Algorithm SHA256).Hash
    Write-Output "Output: $portableExe"
    Write-Output "Bytes: $((Get-Item -LiteralPath $portableExe).Length)"
    Write-Output "SHA-256: $hash"
}
finally {
    Pop-Location
}
