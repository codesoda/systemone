# Install a verified prebuilt s1 release on Windows.
#
#   irm https://raw.githubusercontent.com/codesoda/systemone/main/install.ps1 | iex
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/codesoda/systemone/main/install.ps1))) -Version v0.1.0
#
# The script downloads the x86_64 Windows archive and SHA256SUMS from the
# GitHub release, verifies the archive's SHA-256 and exact contents, and
# installs it under %USERPROFILE%\.systemone\bin without administrator
# rights:
#
#   .systemone\bin\s1-vX.Y.Z-x86_64-pc-windows-msvc\   versioned payload + notices
#   .systemone\bin\s1.exe                               the active version
#
# PATH is changed only with -AddToPath. Everything below is definitions until
# the last line, so a truncated download cannot start an installation.

function Install-SystemOne {
    [CmdletBinding()]
    param(
        [string]$Version = "",
        [switch]$AddToPath,
        # Testing only: install from a local archive and SHA256SUMS instead of
        # downloading. Both are still verified. Requires -Version.
        [string]$LocalArchive = "",
        [string]$LocalChecksums = ""
    )

    Set-StrictMode -Version Latest
    $ErrorActionPreference = "Stop"
    $ProgressPreference = "SilentlyContinue"

    $Repo = "codesoda/systemone"
    $RepoUrl = "https://github.com/$Repo"
    $Target = "x86_64-pc-windows-msvc"
    $MemberFiles = @(
        "s1.exe", "LICENSE", "THIRD_PARTY.md", "THIRD_PARTY_LICENSES.html",
        "RUST-COPYRIGHT-library.html", "colored-3.1.1.crate", "option-ext-0.2.0.crate",
        "README.md", "BUILD-INFO.json"
    )

    function Say([string]$Message) { [Console]::Error.WriteLine("s1 installer: $Message") }
    function Fail([string]$Message) { throw "s1 installer: error: $Message" }

    # PowerShell 5.1 defaults to older TLS versions on some systems.
    try {
        [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
    } catch { }

    if (-not [Environment]::Is64BitOperatingSystem) { Fail "64-bit Windows is required" }
    $arch = $env:PROCESSOR_ARCHITECTURE
    if ($env:PROCESSOR_ARCHITEW6432) { $arch = $env:PROCESSOR_ARCHITEW6432 }
    if ($arch -ne "AMD64") { Fail "unsupported architecture $arch; releases are built for x86_64 (AMD64) Windows" }
    $build = [Environment]::OSVersion.Version.Build
    if ($build -lt 17763) { Fail "Windows 10 version 1809 (build 17763) or newer is required (found build $build)" }
    $tar = Get-Command tar.exe -ErrorAction SilentlyContinue
    if (-not $tar) { Fail "tar.exe was not found; it ships with Windows 10 1803 and newer" }

    $tagPattern = '^v[0-9]+\.[0-9]+\.[0-9]+([-+][0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$'
    if ($LocalArchive -and -not $Version) { Fail "-LocalArchive requires -Version" }
    if ($Version) {
        if ($Version -notmatch $tagPattern) { Fail "-Version must be a tag like v0.1.0" }
        $Tag = $Version
    } else {
        try {
            $latest = Invoke-RestMethod -UseBasicParsing -Uri "https://api.github.com/repos/$Repo/releases/latest" -Headers @{ "User-Agent" = "s1-installer" }
        } catch {
            Fail "could not resolve the latest release: $($_.Exception.Message)"
        }
        $Tag = [string]$latest.tag_name
        if ($Tag -notmatch $tagPattern) { Fail "latest release returned an invalid tag: $Tag" }
    }

    $root = "s1-$Tag-$Target"
    $asset = "$root.tar.gz"
    $base = "$RepoUrl/releases/download/$Tag"
    if (-not $env:USERPROFILE) { Fail "USERPROFILE is not set" }
    $systemOneHome = Join-Path $env:USERPROFILE ".systemone"
    $binDir = Join-Path $systemOneHome "bin"
    New-Item -ItemType Directory -Force -Path $binDir | Out-Null

    $temp = Join-Path ([IO.Path]::GetTempPath()) ("s1-install-" + [Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Path $temp | Out-Null
    try {
        $archive = Join-Path $temp $asset
        $sums = Join-Path $temp "SHA256SUMS"
        if ($LocalArchive) {
            Say "using local $LocalArchive"
            Copy-Item -LiteralPath $LocalArchive -Destination $archive
            Copy-Item -LiteralPath $LocalChecksums -Destination $sums
        } else {
            Say "downloading $asset"
            try {
                Invoke-WebRequest -UseBasicParsing -Uri "$base/$asset" -OutFile $archive
                Invoke-WebRequest -UseBasicParsing -Uri "$base/SHA256SUMS" -OutFile $sums
            } catch {
                Fail "could not download $asset or SHA256SUMS from ${base}: $($_.Exception.Message)"
            }
        }

        # Exactly one well-formed line for this asset.
        $entries = @(Get-Content -LiteralPath $sums | Where-Object { $_ -match "^\s*([0-9A-Fa-f]{64})\s+\*?(\S+)\s*$" -and $Matches[2] -eq $asset })
        if ($entries.Count -ne 1) { Fail "SHA256SUMS must contain exactly one entry for $asset" }
        $expected = ($entries[0] -split '\s+')[0].ToLowerInvariant()
        $actual = (Get-FileHash -Algorithm SHA256 -LiteralPath $archive).Hash.ToLowerInvariant()
        if ($actual -ne $expected) { Fail "SHA-256 mismatch for $asset" }
        Say "verified SHA-256 $actual"

        # The archive must hold exactly the versioned root and its payload.
        $listed = @(& tar.exe -tzf $archive)
        if ($LASTEXITCODE -ne 0) { Fail "invalid release archive: $asset" }
        $wanted = @("$root/") + ($MemberFiles | ForEach-Object { "$root/$_" })
        if (($listed -join "`n") -ne ($wanted -join "`n")) {
            Fail "archive members do not match the exact required release payload"
        }

        $stage = Join-Path $temp "stage"
        New-Item -ItemType Directory -Path $stage | Out-Null
        & tar.exe -xzf $archive -C $stage
        if ($LASTEXITCODE -ne 0) { Fail "could not extract $asset" }
        $staged = Join-Path $stage $root
        $stagedExe = Join-Path $staged "s1.exe"

        $versionJson = & $stagedExe --version
        if ($LASTEXITCODE -ne 0) { Fail "s1.exe --version failed" }
        $reported = ($versionJson | ConvertFrom-Json)
        if ($reported.schema -ne "systemone-version-v1" -or ("v" + $reported.version) -ne $Tag) {
            Fail "downloaded binary reports version $($reported.version), expected $Tag"
        }

        $payload = Join-Path $binDir $root
        if (Test-Path -LiteralPath $payload) {
            foreach ($member in $MemberFiles) {
                $existing = Join-Path $payload $member
                if (-not (Test-Path -LiteralPath $existing -PathType Leaf)) { Fail "existing payload $payload is incomplete; remove it and retry" }
                $a = (Get-FileHash -Algorithm SHA256 -LiteralPath $existing).Hash
                $b = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $staged $member)).Hash
                if ($a -ne $b) { Fail "existing payload differs from the verified release: $member" }
            }
        } else {
            Move-Item -LiteralPath $staged -Destination $payload
        }

        # Activate: copy the verified executable next to the payloads, then
        # replace the active s1.exe in one rename.
        $active = Join-Path $binDir "s1.exe"
        $next = Join-Path $binDir (".s1-next-" + [Guid]::NewGuid().ToString("N") + ".exe")
        Copy-Item -LiteralPath (Join-Path $payload "s1.exe") -Destination $next
        try {
            Move-Item -LiteralPath $next -Destination $active -Force
        } catch {
            Remove-Item -LiteralPath $next -Force -ErrorAction SilentlyContinue
            Fail "could not replace $active (is s1 running?): $($_.Exception.Message)"
        }
    } finally {
        Remove-Item -LiteralPath $temp -Recurse -Force -ErrorAction SilentlyContinue
    }

    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    $onPath = @(($userPath -split ";") + ($env:Path -split ";")) | Where-Object { $_.TrimEnd("\") -ieq $binDir }
    if (-not $onPath) {
        if ($AddToPath) {
            $newPath = if ($userPath) { "$binDir;$userPath" } else { $binDir }
            [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
            $env:Path = "$binDir;$env:Path"
            Say "added $binDir to your user PATH; open a new terminal to use s1"
        } else {
            Say "$binDir is not on PATH. Add it for your user with:"
            Say "  [Environment]::SetEnvironmentVariable('Path', `"$binDir;`" + [Environment]::GetEnvironmentVariable('Path', 'User'), 'User')"
            Say "or rerun the installer with -AddToPath."
        }
    }
    Say "installed $Tag for $Target"
}

Install-SystemOne @args
