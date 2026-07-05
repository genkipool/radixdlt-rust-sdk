# install-connector.ps1 — installs the `radix-connector-mcp` binary from GitHub
# Releases on Windows (no crates.io / npm involved).
#
#   irm https://raw.githubusercontent.com/genkipool/radixdlt-rust-sdk/main/scripts/install-connector.ps1 | iex
#
# Optional: set $env:CONNECTOR_TAG to pin a release tag, and $env:BIN_DIR to
# change the install directory (default: %LOCALAPPDATA%\radix-connector\bin).

$ErrorActionPreference = 'Stop'

$repo   = 'genkipool/radixdlt-rust-sdk'
$binDir = if ($env:BIN_DIR) { $env:BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'radix-connector\bin' }
$target = 'x86_64-pc-windows-msvc'

# Resolve the release tag (latest connector-v* release unless pinned).
$tag = $env:CONNECTOR_TAG
if (-not $tag) {
    Write-Host 'Resolving the latest connector release...'
    $releases = Invoke-RestMethod -Uri "https://api.github.com/repos/$repo/releases" -Headers @{ 'User-Agent' = 'radix-connector-installer' }
    $tag = ($releases | Where-Object { $_.tag_name -like 'connector-v*' } | Select-Object -First 1).tag_name
    if (-not $tag) { throw 'Could not find a connector release. Set $env:CONNECTOR_TAG, or install with cargo (see the README).' }
}

$url  = "https://github.com/$repo/releases/download/$tag/radix-connector-mcp-$target.exe"
$dest = Join-Path $binDir 'radix-connector-mcp.exe'

Write-Host "Downloading radix-connector-mcp ($tag, $target)..."
New-Item -ItemType Directory -Force -Path $binDir | Out-Null
Invoke-WebRequest -Uri $url -OutFile $dest -Headers @{ 'User-Agent' = 'radix-connector-installer' }

Write-Host ''
Write-Host "Installed: $dest"
if (($env:PATH -split ';') -notcontains $binDir) {
    Write-Host "NOTE: $binDir is not on your PATH. Add it for the current user with:"
    Write-Host "      setx PATH `"$binDir;`$env:PATH`""
}
Write-Host ''
Write-Host 'Register it with your MCP client, e.g. Claude Code:'
Write-Host "  claude mcp add radix-connector -- `"$dest`""
Write-Host ''
Write-Host 'Or in a JSON MCP config:'
Write-Host "  { `"mcpServers`": { `"radix-connector`": { `"command`": `"$($dest -replace '\\','\\')`" } } }"
