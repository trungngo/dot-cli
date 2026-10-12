<#
.SYNOPSIS
Installs dot from the latest GitHub release, then (optionally) clones a dotfiles repository and
applies it. The Windows counterpart of install.sh.

.DESCRIPTION
dot's releases are public: installing dot needs no token (Windows PowerShell 5.1 and
PowerShell 7):

  iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex

  # and a dotfiles repository (iex takes no arguments, so the URL goes in the environment):
  $env:DOT_DOTFILES_URL = 'https://github.com/you/dotfiles.git'
  iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex

  # or, from a saved copy:
  .\install.ps1 https://github.com/you/dotfiles.git

  # a pinned release: the script from that release's tag, and the same tag in DOT_VERSION
  $env:DOT_VERSION = 'v0.1.0'
  iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/v0.1.0/install.ps1 | iex

A GitHub token is only used when something private is asked for: a private dotfiles repository
on github.com (the token goes to git only when it cannot read the repository with its own
credentials), or releases from a private fork (DOT_RELEASE_REPO=owner/name; the token goes to
the GitHub API only when the public download URL fails). For a private dotfiles repository,
without the token in PowerShell's history or on any command line:

  $env:GITHUB_TOKEN = [Net.NetworkCredential]::new('', (Read-Host 'GitHub token' -AsSecureString)).Password
  $env:DOT_DOTFILES_URL = 'https://github.com/you/dotfiles.git'
  iwr -useb https://raw.githubusercontent.com/trungngo/dot-cli/main/install.ps1 | iex
  Remove-Item Env:GITHUB_TOKEN

1. The token, when needed: GITHUB_TOKEN, else GH_TOKEN, else `gh auth token` when the GitHub
   CLI is installed and logged in. A fine-grained personal access token with read-only
   "Contents" on the private repositories is enough.
2. Picks the release asset for this CPU: dot-x86_64-pc-windows-msvc.zip or
   dot-aarch64-pc-windows-msvc.zip.
3. Downloads it and the release's SHA256SUMS from the public download URL, without any
   credentials (or, when that fails and there is a token, through the GitHub API: the latest
   release, then each asset by id), and refuses to install unless the checksum matches.
   Every download goes through System.Net.Http.HttpClient with redirects followed here, the same
   way in PowerShell 5.1 and 7 (Invoke-WebRequest differs between them): a redirect may not leave
   https, and the Authorization header is dropped when one leads to another host (the API sends
   assets to a storage host). The checksum comes from the same release (and host) as the binary,
   so it only catches a corrupted or truncated download: it does not prove who built the binary.
   Trust rests on TLS to github.com (a DOT_RELEASE_BASE, DOT_GITHUB_URL or DOT_GITHUB_API that is
   not https prints a warning).
4. Installs dot.exe into %LOCALAPPDATA%\Programs\dot and adds that to the user's PATH (unless
   DOT_NO_MODIFY_PATH is set).
5. With a repository URL (https:// with no credentials in it, or ssh: ssh://... or
   git@host:owner/name.git, which use your ssh keys and never the token): clones it to
   ~\.dot (or pulls it, fast-forward only, when it is already there), records it as
   DOT_REPO in ~\.config\dot\local.env (unless another DOT_REPO is recorded there: that one is
   kept, with a warning), and runs `dot apply`. For a github.com https URL that git cannot read with
   its own credentials, the token goes to git as an `http.https://github.com/.extraheader` in
   git's environment (GIT_CONFIG_COUNT, git 2.31 or later), removed again afterwards: nothing is
   written to any git config, so the checkout keeps no credentials and later pulls use git's own
   (Git Credential Manager, or `gh auth setup-git`).

The token never goes on a command line, into a URL, a file or a message. `dot apply` runs
without it: GITHUB_TOKEN and GH_TOKEN are removed from the environment while it runs (and put
back afterwards).

Environment: GITHUB_TOKEN, GH_TOKEN (a token, for private repositories only), DOT_VERSION (the
release to install, e.g. v0.1.0; default the latest), DOT_INSTALL_DIR
(where dot.exe goes), DOT_DOTFILES_DIR (where the repository is cloned, default ~\.dot),
DOT_DOTFILES_URL (the repository, when piping into iex, which takes no arguments),
DOT_NO_MODIFY_PATH (leave the user's PATH alone), DOT_RELEASE_REPO (the GitHub repository dot
is released from, default trungngo/dot-cli), DOT_RELEASE_BASE (where the release assets
are, fetched without a token: mirrors, tests), DOT_GITHUB_URL (the GitHub web host, default
https://github.com),
DOT_GITHUB_API (the GitHub API, default https://api.github.com), DOT_APPLY_ARGS (extra
arguments for `dot apply`, space-separated), XDG_CONFIG_HOME (where local.env goes:
$XDG_CONFIG_HOME\dot\local.env, default ~\.config).

Works in Windows PowerShell 5.1 and PowerShell 7. Needs no administrator rights. Errors are
thrown, never `exit`: piped into iex, `exit` would close the caller's terminal.
#>
param([string]$RepoUrl = $env:DOT_DOTFILES_URL)

function Install-Dot {
    param([string]$RepoUrl)
    Set-StrictMode -Version Latest
    $ErrorActionPreference = 'Stop'
    $ProgressPreference = 'SilentlyContinue' # progress bars (Expand-Archive) are slow
    [Net.ServicePointManager]::SecurityProtocol =
        [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

    $defaultRepo = 'trungngo/dot-cli'
    $repo = if ($env:DOT_RELEASE_REPO) { $env:DOT_RELEASE_REPO } else { $defaultRepo }
    if ($repo -notmatch '^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$' -or $repo -match '(^|/)\.\.?(/|$)') {
        throw "DOT_RELEASE_REPO must be owner/name (got: $repo)"
    }
    $web = if ($env:DOT_GITHUB_URL) { $env:DOT_GITHUB_URL.TrimEnd('/') } else {
        'https://github.com'
    }
    $version = $env:DOT_VERSION
    if ($version -and ($version -notmatch '^v[0-9][A-Za-z0-9._+-]*$')) {
        throw "DOT_VERSION must be a release tag such as v0.1.0 (got: $version)"
    }
    if ($version) {
        $publicBase = "$web/$repo/releases/download/$version"
        $apiRelease = "releases/tags/$version"
        $rawUrl = "https://raw.githubusercontent.com/$repo/$version/install.ps1"
    } else {
        $publicBase = "$web/$repo/releases/latest/download"
        $apiRelease = 'releases/latest'
        $rawUrl = "https://raw.githubusercontent.com/$repo/main/install.ps1"
    }
    $api = if ($env:DOT_GITHUB_API) { $env:DOT_GITHUB_API.TrimEnd('/') } else {
        'https://api.github.com'
    }
    $binDir = if ($env:DOT_INSTALL_DIR) { $env:DOT_INSTALL_DIR } else {
        Join-Path $env:LOCALAPPDATA 'Programs\dot'
    }
    $dotfilesDir = if ($env:DOT_DOTFILES_DIR) { $env:DOT_DOTFILES_DIR } else {
        Join-Path $HOME '.dot'
    }
    $configHome = if ($env:XDG_CONFIG_HOME) { $env:XDG_CONFIG_HOME } else {
        Join-Path $HOME '.config'
    }

    function Say([string]$msg) { Write-Host "install.ps1: $msg" }

    # Sets, or for $null removes, a variable of this process's environment. Not through
    # [Environment]::SetEnvironmentVariable: PowerShell hands it $null as '', which PowerShell 7.5
    # (.NET 9) on Linux and macOS keeps as an empty variable (an empty GIT_TERMINAL_PROMPT would
    # turn git's prompts off for the rest of the session).
    function Set-ProcessEnv([string]$Name, $Value) {
        if ($null -eq $Value) {
            Remove-Item -Path "Env:$Name" -ErrorAction SilentlyContinue
        } else {
            Set-Item -Path "Env:$Name" -Value $Value
        }
    }

    # How to give this script a token, for when something private ($What) failed without one.
    function Get-TokenHelp([string]$What) {
        $url = if ($RepoUrl) { $RepoUrl } else { 'https://github.com/you/dotfiles.git' }
        $pin = if ($version) { "`$env:DOT_VERSION = '$version'`n     " } else { '' }
        $fetch = if ($repo -eq $defaultRepo) {
            "${pin}iwr -useb $rawUrl | iex"
        } else {
            # A private fork's raw file needs the token too (in-process: no argv to leak).
            "$pin`$env:DOT_RELEASE_REPO = '$repo'`n     " +
            "iwr -useb -Headers @{ Authorization = `"Bearer `$env:GITHUB_TOKEN`" } $rawUrl | iex"
        }
        @"

If $What is private, this script needs a GitHub token that can read it.

1. Create a fine-grained personal access token:
     https://github.com/settings/personal-access-tokens/new
   Repository access: "Only select repositories": the private repositories this needs.
   Repository permissions: Contents: Read-only (Metadata: Read-only is added by itself).
   Pick a short expiry.

2. Run, in PowerShell (the token is read masked: it stays out of PowerShell's history and off
   every command line):

     `$env:GITHUB_TOKEN = [Net.NetworkCredential]::new('', (Read-Host 'GitHub token' -AsSecureString)).Password
     `$env:DOT_DOTFILES_URL = '$url'
     $fetch
     Remove-Item Env:GITHUB_TOKEN

Or install the GitHub CLI (winget install GitHub.cli) and run 'gh auth login': this script then
uses 'gh auth token'.
"@
    }

    # Checked before anything is downloaded.
    # https:// (no credentials in it), ssh://, or scp-like user@host:path. An ssh URL uses the
    # ssh keys (and ssh's own prompts); the token is never used for it.
    if ($RepoUrl) {
        if ($RepoUrl -match '^-|://-|@-') {
            throw "the repository URL must not start with a dash (got: $RepoUrl)"
        }
        if ($RepoUrl -match '^https://[^/]*@') {
            throw 'put no credentials in the URL; set GITHUB_TOKEN or use a git credential helper'
        }
        if ($RepoUrl -match '^ssh://[^/]*:[^/]*@') {
            throw 'put no password in the URL; ssh uses your keys'
        }
        if ($RepoUrl -notmatch '^(https|ssh)://.' -and
            ($RepoUrl -match '://' -or $RepoUrl -notmatch '^[^@:/\\]+@[^@:/\\]+:.')) {
            throw "the repository URL must be https://, ssh:// or user@host:path (got: $RepoUrl)"
        }
        if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
            throw "git is needed to clone $RepoUrl; install it first (winget install Git.Git)"
        }
    }

    # The GitHub token, and where it came from (for messages; never the value): looked up once,
    # when first needed, by Find-Token.
    $tok = @{ Looked = $false; Value = $null; From = $null }
    function Find-Token {
        if ($tok.Looked) { return }
        $tok.Looked = $true
        foreach ($name in 'GITHUB_TOKEN', 'GH_TOKEN') {
            $v = [Environment]::GetEnvironmentVariable($name)
            if ($v -and $v.Trim()) {
                $tok.Value = $v.Trim()
                $tok.From = $name
                break
            }
        }
        if (-not $tok.Value -and (Get-Command gh -ErrorAction SilentlyContinue)) {
            # Windows PowerShell turns redirected native stderr into errors, fatal under 'Stop'.
            $out = & {
                $ErrorActionPreference = 'Continue'
                gh auth token --hostname github.com 2>$null
            }
            if ($LASTEXITCODE -eq 0 -and $out) {
                $tok.Value = ([string]@($out)[0]).Trim()
                $tok.From = 'gh auth token'
            }
        }
        # GitHub tokens are letters, digits and underscores (dots and dashes are let through
        # too); anything else would break the header.
        if ($tok.Value -and $tok.Value -notmatch '^[A-Za-z0-9_.-]+$') {
            throw "the GitHub token from $($tok.From) holds whitespace or other unexpected characters"
        }
    }

    $cpu = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else {
        $env:PROCESSOR_ARCHITECTURE
    }
    $arch = switch ($cpu) {
        'AMD64' { 'x86_64' }
        'ARM64' { 'aarch64' }
        default { throw "unsupported CPU: $cpu" }
    }
    $asset = "dot-$arch-pc-windows-msvc.zip"

    # Every download goes through HttpClient, which follows no redirect itself: Save-Url does,
    # the same way in Windows PowerShell 5.1 and PowerShell 7 (whose Invoke-WebRequest differ on
    # redirects and on what they keep sending along).
    Add-Type -AssemblyName System.Net.Http
    $handler = [System.Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [System.Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromMinutes(10)
    [void]$client.DefaultRequestHeaders.UserAgent.TryParseAdd('dot-install')

    # GETs $Url into $OutFile, following up to 10 redirects: never from https to another scheme,
    # and never with the Authorization header to another host (the API sends release assets to
    # a storage host, whose signed URL is the permission). Returns the final HTTP status; the
    # file is written only for a 2xx. Messages name URLs without their query (a signed URL's
    # signature).
    function Save-Url([string]$Url, [string]$OutFile, [hashtable]$Headers = @{}) {
        $sent = @{} + $Headers
        $uri = [Uri]$Url
        for ($i = 0; $i -le 10; $i++) {
            $shown = $uri.GetLeftPart([UriPartial]::Path)
            $req = [System.Net.Http.HttpRequestMessage]::new([System.Net.Http.HttpMethod]::Get, $uri)
            foreach ($k in $sent.Keys) {
                [void]$req.Headers.TryAddWithoutValidation($k, [string]$sent[$k])
            }
            try {
                $resp = $client.SendAsync($req,
                    [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
            } catch {
                throw "cannot download ${shown}: $($_.Exception.GetBaseException().Message)"
            } finally {
                $req.Dispose()
            }
            try {
                $code = [int]$resp.StatusCode
                if ($code -in 301, 302, 303, 307, 308) {
                    $location = $resp.Headers.Location
                    if (-not $location) { throw "a redirect without a location from $shown" }
                    $next = if ($location.IsAbsoluteUri) { $location } else {
                        [Uri]::new($uri, $location)
                    }
                    if ($uri.Scheme -eq 'https' -and $next.Scheme -ne 'https') {
                        throw "refusing the redirect from $shown away from https"
                    }
                    if ($next.GetLeftPart([UriPartial]::Authority) -ne
                        $uri.GetLeftPart([UriPartial]::Authority)) {
                        $sent.Remove('Authorization')
                    }
                    $uri = $next
                    continue
                }
                if ($code -lt 200 -or $code -ge 300) { return $code }
                $in = $resp.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
                $fs = [IO.File]::Create($OutFile)
                try { $in.CopyTo($fs) } finally { $fs.Dispose(); $in.Dispose() }
                return $code
            } finally {
                $resp.Dispose()
            }
        }
        throw "more than 10 redirects from $Url"
    }

    function Get-ApiError([int]$Code, [string]$What) {
        switch ($Code) {
            401 { "GitHub rejected the token from $($tok.From) (401): it is wrong or expired" }
            { $_ -in 403, 404 } {
                "${What}: HTTP ${Code}: the token from $($tok.From) cannot read $repo (a " +
                'fine-grained token needs read-only "Contents" on it)'
            }
            default { "${What}: HTTP $Code" }
        }
    }

    # Adds $binDir to the user's PATH (and this session's).
    function Add-UserPath([string]$binDir) {
        # The user PATH is read and written raw, as a REG_EXPAND_SZ: going through
        # [Environment]::Get/SetEnvironmentVariable would expand every %VAR% entry for good (and
        # Windows PowerShell 5.1 writes the value back as REG_SZ, so entries added later with %VAR%
        # would no longer expand either).
        $envKey = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey('Environment', $true)
        if (-not $envKey) {
            $envKey = [Microsoft.Win32.Registry]::CurrentUser.CreateSubKey('Environment')
        }
        try {
            $userPath = [string]$envKey.GetValue('Path', '',
                [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
            $entries = @(if ($userPath) { $userPath -split ';' | Where-Object { $_ } } else { @() })
            $expanded = @($entries | ForEach-Object {
                    [Environment]::ExpandEnvironmentVariables($_).TrimEnd('\')
                })
            if ($expanded -notcontains $binDir.TrimEnd('\')) {
                $newPath = (@($entries) + $binDir) -join ';'
                $envKey.SetValue('Path', $newPath, [Microsoft.Win32.RegistryValueKind]::ExpandString)
                # Tell Explorer (and so new terminals) that the environment changed: setting a
                # variable through .NET broadcasts WM_SETTINGCHANGE; this one is removed again.
                [Environment]::SetEnvironmentVariable('DOT_INSTALL_PATH_CHANGED', '1', 'User')
                [Environment]::SetEnvironmentVariable('DOT_INSTALL_PATH_CHANGED', $null, 'User')
                Say "added $binDir to your PATH (new terminals see it)"
            }
        } finally {
            $envKey.Close()
        }
        if (($env:Path -split ';') -notcontains $binDir) { $env:Path = "$env:Path;$binDir" }
    }

    $tmp = Join-Path ([IO.Path]::GetTempPath()) ("dot-install-" + [Guid]::NewGuid())
    New-Item -ItemType Directory -Path $tmp | Out-Null
    try {
        Say "downloading $asset"
        $zip = Join-Path $tmp $asset
        $sums = Join-Path $tmp 'SHA256SUMS'
        $fromApi = $false
        if ($env:DOT_RELEASE_BASE) {
            $base = $env:DOT_RELEASE_BASE
            if ($base -notmatch '^https://') {
                Write-Warning ("install.ps1: DOT_RELEASE_BASE is not https ($base): the " +
                    'download and its checksum are not protected in transit')
            }
            foreach ($f in @(@($asset, $zip), @('SHA256SUMS', $sums))) {
                $code = Save-Url "$base/$($f[0])" $f[1]
                if ($code -ne 200) { throw "cannot download $base/$($f[0]) (HTTP $code)" }
            }
        } else {
            if ($web -notmatch '^https://') {
                Write-Warning ("install.ps1: DOT_GITHUB_URL is not https ($web): the " +
                    'download and its checksum are not protected in transit')
            }
            $code = Save-Url "$publicBase/$asset" $zip
            if ($code -eq 200) {
                $code = Save-Url "$publicBase/SHA256SUMS" $sums
                if ($code -ne 200) { throw "cannot download $publicBase/SHA256SUMS (HTTP $code)" }
            } else {
                Find-Token
                if (-not $tok.Value) {
                    $why = if ($version) { "no release $version" } else { 'no published release yet' }
                    throw ("cannot download $publicBase/$asset (HTTP ${code}: $why, or $repo " +
                        "is private)`n$(Get-TokenHelp $repo)")
                }
                Say "cannot download $publicBase/$asset (HTTP $code); trying the GitHub API"
                $fromApi = $true
            }
        }
        if ($fromApi) {
            if ($api -notmatch '^https://') {
                Write-Warning ("install.ps1: DOT_GITHUB_API is not https ($api): the token and " +
                    'the download are not protected in transit')
            }
            Say "using the GitHub token from $($tok.From) for the release of $repo"
            $auth = @{
                Authorization = "Bearer $($tok.Value)"
                'X-GitHub-Api-Version' = '2022-11-28'
            }
            $json = Join-Path $tmp 'release.json'
            $code = Save-Url "$api/repos/$repo/$apiRelease" $json ($auth + @{
                    Accept = 'application/vnd.github+json'
                })
            if ($code -eq 404) {
                # The token can read the repository, but it has no release (yet)? Otherwise 404
                # is what GitHub answers a token that cannot read it.
                $code = Save-Url "$api/repos/$repo" (Join-Path $tmp 'repo.json') ($auth + @{
                        Accept = 'application/vnd.github+json'
                    })
                if ($code -eq 200 -and $version) {
                    throw ("$repo has no release $version (the token from $($tok.From) can " +
                        'read the repository)')
                }
                if ($code -eq 200) {
                    throw ("$repo has no published release yet (the token from $($tok.From) " +
                        'can read the repository); publish one, or set DOT_RELEASE_BASE to a mirror')
                }
                $code = 404
            }
            if ($code -ne 200) {
                $which = if ($version) { $version } else { 'latest' }
                throw (Get-ApiError $code "looking up release $which of $repo")
            }
            $release = [IO.File]::ReadAllText($json) | ConvertFrom-Json
            $tag = $release.tag_name
            $found = @{}
            foreach ($a in @($release.assets)) { $found[[string]$a.name] = $a.id }
            if (-not $found.ContainsKey($asset)) { throw "release $tag of $repo has no $asset" }
            if (-not $found.ContainsKey('SHA256SUMS')) {
                throw "release $tag of $repo has no SHA256SUMS: refusing to install an unverified binary"
            }
            Say "release $tag"
            foreach ($f in @(@($asset, $zip), @('SHA256SUMS', $sums))) {
                $code = Save-Url "$api/repos/$repo/releases/assets/$($found[$f[0]])" $f[1] ($auth + @{
                        Accept = 'application/octet-stream'
                    })
                if ($code -ne 200) { throw (Get-ApiError $code "downloading $($f[0])") }
            }
        }
        # Lines are "<sha256>  <file>" (or "<sha256> *<file>").
        $want = $null
        foreach ($line in Get-Content $sums) {
            $parts = $line.Trim() -split '\s+', 2
            if ($parts.Count -eq 2 -and $parts[1].TrimStart('*') -eq $asset) {
                $want = $parts[0].ToLowerInvariant()
                break
            }
        }
        if (-not $want) { throw "SHA256SUMS lists no checksum for $asset" }
        $have = (Get-FileHash -Algorithm SHA256 -Path $zip).Hash.ToLowerInvariant()
        if ($have -ne $want) {
            throw "checksum mismatch for $asset (expected $want, got $have)"
        }
        $unpacked = Join-Path $tmp 'x'
        Expand-Archive -Path $zip -DestinationPath $unpacked
        $exe = Get-ChildItem -Path $unpacked -Recurse -Filter 'dot.exe' | Select-Object -First 1
        if (-not $exe) { throw "$asset holds no dot.exe" }
        New-Item -ItemType Directory -Force -Path $binDir | Out-Null
        $dest = Join-Path $binDir 'dot.exe'
        # Copied next to the destination, then renamed over it.
        $staged = Join-Path $binDir ".dot.exe.$PID.tmp"
        Copy-Item -Path $exe.FullName -Destination $staged -Force
        try {
            Move-Item -Path $staged -Destination $dest -Force
        } catch {
            Remove-Item -Path $staged -Force -ErrorAction SilentlyContinue
            throw "cannot replace $dest (is dot running?): $_"
        }
    } finally {
        Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
        $client.Dispose()
    }
    Say "installed $(& $dest --version) to $dest"
    if ($env:DOT_NO_MODIFY_PATH) {
        if (($env:Path -split ';') -notcontains $binDir) { $env:Path = "$env:Path;$binDir" }
    } else {
        Add-UserPath $binDir
    }

    if (-not $RepoUrl) { return }

    # For github.com, the token goes to git as an extra header in its environment, set for the
    # clone (or pull) only: never on its command line, never into a config file. Only when git
    # cannot read the repository with its own credentials (a public repository, or Git
    # Credential Manager that can, needs no token), asked without prompting.
    $gitEnv = @{}
    if ($RepoUrl -match '^https://github\.com/') {
        $probeEnv = @{ GIT_TERMINAL_PROMPT = '0'; GCM_INTERACTIVE = 'never' }
        $saved = @{}
        foreach ($k in $probeEnv.Keys) {
            $saved[$k] = [Environment]::GetEnvironmentVariable($k)
            Set-ProcessEnv $k $probeEnv[$k]
        }
        try {
            # Windows PowerShell turns redirected native stderr into errors, fatal under 'Stop'.
            & {
                $ErrorActionPreference = 'Continue'
                git ls-remote -q $RepoUrl HEAD 2>$null | Out-Null
            }
            $readable = $LASTEXITCODE -eq 0
        } finally {
            foreach ($k in $saved.Keys) { Set-ProcessEnv $k $saved[$k] }
        }
        if (-not $readable) { Find-Token }
        if (-not $readable -and $tok.Value) {
            $version = [string](& git version)
            if ($version -notmatch 'git version (\d+)\.(\d+)' -or [int]$Matches[1] -lt 2 -or
                ([int]$Matches[1] -eq 2 -and [int]$Matches[2] -lt 31)) {
                throw ("$version cannot take the token from its environment (needs 2.31 or " +
                    'later); upgrade git (winget upgrade Git.Git), or set up a credential helper ' +
                    'and remove GITHUB_TOKEN')
            }
            Say "$RepoUrl needs credentials: using the GitHub token from $($tok.From)"
            $basic = [Convert]::ToBase64String(
                [Text.Encoding]::ASCII.GetBytes("x-access-token:$($tok.Value)"))
            $gitEnv = @{
                GIT_CONFIG_COUNT = '1'
                GIT_CONFIG_KEY_0 = 'http.https://github.com/.extraheader'
                GIT_CONFIG_VALUE_0 = "Authorization: Basic $basic"
                GIT_TERMINAL_PROMPT = '0'
            }
        }
    }
    # Runs git with $gitEnv, restoring this session's environment afterwards; returns its exit
    # code.
    function Invoke-Git([string[]]$GitArgs) {
        $saved = @{}
        foreach ($k in $gitEnv.Keys) {
            $saved[$k] = [Environment]::GetEnvironmentVariable($k)
            Set-ProcessEnv $k $gitEnv[$k]
        }
        try {
            & git @GitArgs | Out-Host
            $LASTEXITCODE
        } finally {
            foreach ($k in $saved.Keys) { Set-ProcessEnv $k $saved[$k] }
        }
    }

    if (Test-Path (Join-Path $dotfilesDir '.git')) {
        # Windows PowerShell turns redirected native stderr into errors, fatal under 'Stop'.
        $origin = & {
            $ErrorActionPreference = 'Continue'
            git -C $dotfilesDir remote get-url origin 2>$null
        }
        if ($origin -ne $RepoUrl) {
            throw "$dotfilesDir is a checkout of $origin, not $RepoUrl; set DOT_DOTFILES_DIR"
        }
        Say "updating $dotfilesDir"
        if ((Invoke-Git @('-C', $dotfilesDir, 'pull', '--ff-only')) -ne 0) {
            throw "git pull failed in $dotfilesDir"
        }
    } elseif (Test-Path $dotfilesDir) {
        throw "$dotfilesDir exists and is not a git checkout; set DOT_DOTFILES_DIR"
    } else {
        Say "cloning $RepoUrl to $dotfilesDir"
        if ((Invoke-Git @('clone', '--', $RepoUrl, $dotfilesDir)) -ne 0) {
            $help = if ($tok.Value -or $RepoUrl -notmatch '^https://github\.com/') { '' } else {
                Get-TokenHelp $RepoUrl
            }
            throw "git clone $RepoUrl failed$help"
        }
    }

    $localEnv = Join-Path $configHome 'dot\local.env'
    $recorded = if (Test-Path $localEnv) {
        Select-String -Path $localEnv -Pattern '^DOT_REPO=(.*)$' | Select-Object -Last 1 |
            ForEach-Object { $_.Matches[0].Groups[1].Value.Trim().Trim("'", '"') }
    }
    if ($recorded) {
        # As dot reads it: ~ is the home directory.
        $shown = if ($recorded -match '^~[\\/]') { Join-Path $HOME $recorded.Substring(2) } else {
            $recorded
        }
        if ($shown.TrimEnd('\', '/') -ne $dotfilesDir.TrimEnd('\', '/')) {
            Write-Warning ("install.ps1: $localEnv already has DOT_REPO=$shown, left as it is: " +
                "this run applies $dotfilesDir, but later dot commands outside a repository use " +
                "$shown; edit DOT_REPO there to switch")
        }
    } else {
        New-Item -ItemType Directory -Force -Path (Split-Path $localEnv) | Out-Null
        # Single-quoted: dotenv takes backslashes literally only there. UTF-8 without a byte
        # order mark (Windows PowerShell's -Encoding utf8 adds one).
        $line = "DOT_REPO='$dotfilesDir'`n"
        [IO.File]::AppendAllText($localEnv, $line, (New-Object Text.UTF8Encoding $false))
        Say "recorded DOT_REPO=$dotfilesDir in $localEnv"
    }

    Say 'running dot apply'
    $extra = @(if ($env:DOT_APPLY_ARGS) { $env:DOT_APPLY_ARGS -split '\s+' | Where-Object { $_ } })
    # The token was for this script: `dot apply` (and everything it runs, such as gh, which
    # would prefer it to its own login) does not get it. Put back afterwards: piped into iex,
    # this is the caller's session.
    $savedTokens = @{}
    foreach ($k in 'GITHUB_TOKEN', 'GH_TOKEN') {
        $savedTokens[$k] = [Environment]::GetEnvironmentVariable($k)
        Set-ProcessEnv $k $null
    }
    try {
        & $dest apply --repo $dotfilesDir @extra
        $applyCode = $LASTEXITCODE
    } finally {
        foreach ($k in $savedTokens.Keys) { Set-ProcessEnv $k $savedTokens[$k] }
    }
    if ($applyCode -ne 0) { throw "dot apply exited with code $applyCode" }
}

Install-Dot -RepoUrl $RepoUrl
