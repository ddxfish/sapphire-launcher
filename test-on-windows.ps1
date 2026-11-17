<#
Sapphire Launcher - Windows check

What it does
  Looks at this PC and at the launcher, runs the launcher's headless switches, and
  writes a report you can paste to the people testing with you.

What it never does
  - It sends nothing anywhere. The only network call is to this PC itself (127.0.0.1).
  - It never reads your API keys, your password file, your chats or Sapphire's own files.
  - The report has no user name and no PC name: paths are written as %USERPROFILE% and so on.
  - It installs nothing and changes no setting.

The second part is optional and asks first: it starts the local LLM, checks it, and
stops it again. It is skipped by itself when Sapphire or the LLM is already running.

How to run
  Double-click test-on-windows.bat (next to this file), or in PowerShell:
      powershell -ExecutionPolicy Bypass -File .\test-on-windows.ps1
  Options:
      -Exe <path>     the launcher to test (default: sapphire-launcher.exe next to this file)
      -Active         run the LLM start/stop part without asking
      -ReadOnly       never run the LLM start/stop part
      -NoQuestions    skip the questions at the end
#>
param(
    [string]$Exe = "",
    [switch]$Active,
    [switch]$ReadOnly,
    [switch]$NoQuestions
)

$ErrorActionPreference = 'Continue'
$ScriptVersion = '1'
$Here = Split-Path -Parent $MyInvocation.MyCommand.Path
$Lines = New-Object System.Collections.Generic.List[string]
$Counts = @{ PASS = 0; FAIL = 0; INFO = 0; SKIP = 0 }

# ---- the report: every line goes through Redact ---------------------------------

# Folders first (longest first), then the bare names. More folders are added below
# as they are learned: where the launcher is, and a models folder of your own.
$Folders = New-Object System.Collections.Generic.List[object]
function Hide-Folder([string]$path, [string]$label) {
    # Never a drive root: that would rewrite every path on the drive.
    if ($path -and $path.TrimEnd('\').Length -ge 4) { $Folders.Add(@($path.TrimEnd('\'), $label)) }
}
Hide-Folder $env:LOCALAPPDATA '%LOCALAPPDATA%'
Hide-Folder $env:APPDATA '%APPDATA%'
Hide-Folder $env:USERPROFILE '%USERPROFILE%'

function Redact([string]$text) {
    if ($null -eq $text) { return '' }
    foreach ($pair in $Folders) {
        # As written, with doubled backslashes (JSON), and with forward slashes.
        foreach ($spelling in @($pair[0], $pair[0].Replace('\', '\\'), $pair[0].Replace('\', '/'))) {
            $text = [regex]::Replace($text, [regex]::Escape($spelling), $pair[1], 'IgnoreCase')
        }
    }
    # The names on their own, as whole words only.
    foreach ($pair in @(@($env:COMPUTERNAME, '<pc>'), @($env:USERNAME, '<user>'))) {
        if ($pair[0] -and $pair[0].Length -ge 3) {
            $whole = '(?<![A-Za-z0-9_%])' + [regex]::Escape($pair[0]) + '(?![A-Za-z0-9_%])'
            $text = [regex]::Replace($text, $whole, $pair[1], 'IgnoreCase')
        }
    }
    return $text
}

function Say([string]$text) {
    $safe = Redact $text
    $Lines.Add($safe)
    Write-Host $safe
}

function Check([string]$verdict, [string]$name, [string]$detail) {
    $Counts[$verdict] = $Counts[$verdict] + 1
    $line = '[' + $verdict + '] ' + $name
    if ($detail) { $line = $line + ' - ' + $detail }
    Say $line
}

function Section([string]$title) {
    Say ''
    Say ('== ' + $title + ' ==')
}

# ---- running things -----------------------------------------------------------

# Run the launcher with a pipe on its output, the way Sapphire's core calls it.
# PipeHeld: the launcher ended but something it started still holds our pipe.
function Run-Launcher([string]$arguments, [int]$timeoutSec) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $ExePath
    $psi.Arguments = $arguments
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    $result = @{ Exited = $false; PipeHeld = $false; Code = -1; Out = ''; Ms = 0; Json = $null }
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    try {
        $p = [System.Diagnostics.Process]::Start($psi)
    } catch {
        $result.Out = 'could not start: ' + $_.Exception.Message
        return $result
    }
    $outTask = $p.StandardOutput.ReadToEndAsync()
    $errTask = $p.StandardError.ReadToEndAsync()
    $result.Exited = $p.WaitForExit($timeoutSec * 1000)
    $watch.Stop()
    $result.Ms = [int]$watch.ElapsedMilliseconds
    if (-not $result.Exited) {
        # Only ever the process this script started itself.
        try { $p.Kill() } catch { }
        return $result
    }
    $result.Code = $p.ExitCode
    if ($outTask.Wait(5000)) {
        $result.Out = $outTask.Result
    } else {
        $result.PipeHeld = $true
    }
    if ($result.Out) {
        try { $result.Json = $result.Out.Trim() | ConvertFrom-Json } catch { }
    }
    return $result
}

function Get-Status {
    $r = Run-Launcher '--llm-status' 30
    return $r.Json
}

function Tool-Version([string]$name, [string]$argument) {
    $cmd = Get-Command $name -ErrorAction SilentlyContinue
    if (-not $cmd) { return $null }
    try {
        $out = & $cmd.Source $argument 2>$null | Select-Object -First 1
        if ($out) { return [string]$out }
    } catch { }
    return 'found'
}

function Nvidia-Line {
    $smi = Get-Command 'nvidia-smi' -ErrorAction SilentlyContinue
    if (-not $smi) { return @() }
    try {
        return @(& $smi.Source '--query-gpu=name,memory.total,memory.used,driver_version' '--format=csv,noheader' 2>$null)
    } catch { return @() }
}

function Free-Port {
    $listener = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Loopback, 0)
    $listener.Start()
    $port = $listener.LocalEndpoint.Port
    $listener.Stop()
    return $port
}

# 200 = ready, 503 = up and still loading, 0 = no answer.
function Health-Code([int]$port) {
    try {
        $r = Invoke-WebRequest -UseBasicParsing -Uri ('http://127.0.0.1:' + $port + '/health') -TimeoutSec 4
        return [int]$r.StatusCode
    } catch {
        if ($_.Exception.Response) { return [int]$_.Exception.Response.StatusCode }
        return 0
    }
}

function Ask-YesNo([string]$question) {
    $answer = Read-Host ($question + ' [y/N]')
    return ($answer -match '^(y|yes)$')
}

# ---- find the launcher ----------------------------------------------------------

$ExePath = $Exe
if (-not $ExePath) {
    foreach ($candidate in @((Join-Path $Here 'sapphire-launcher.exe'), (Join-Path $Here 'dist\sapphire-launcher.exe'))) {
        if (Test-Path $candidate) { $ExePath = $candidate; break }
    }
}
if (-not $ExePath -or -not (Test-Path $ExePath)) {
    Write-Host 'Could not find sapphire-launcher.exe next to this script.'
    Write-Host 'Put this script in the same folder as the launcher, or run it with:  -Exe <path to sapphire-launcher.exe>'
    exit 2
}
$ExePath = (Resolve-Path $ExePath).Path
Hide-Folder (Split-Path -Parent $ExePath) '<launcher folder>'

Say ('Sapphire Launcher - Windows check, script version ' + $ScriptVersion)
Say ('Run on ' + (Get-Date -Format 'yyyy-MM-dd HH:mm'))
Say 'This report has no user name, PC name, key or chat in it. Read it before you share it.'

# ---- 1. this PC -----------------------------------------------------------------

Section 'This PC'
try {
    $os = Get-CimInstance Win32_OperatingSystem
    Say ('Windows: ' + $os.Caption + ', build ' + $os.BuildNumber + ', ' + $os.OSArchitecture)
} catch { Say 'Windows: could not read the version' }
Say ('PowerShell: ' + $PSVersionTable.PSVersion.ToString())

$plain = '^[\x20-\x7E]+$'
$nameAscii = [bool]($env:USERNAME -match $plain)
$profileAscii = [bool]($env:USERPROFILE -match $plain)
$profileSpace = [bool]($env:USERPROFILE -match ' ')
Say ('Account name: plain letters only = ' + $nameAscii + '; home folder: plain letters only = ' + $profileAscii + ', has a space = ' + $profileSpace + ', length = ' + $env:USERPROFILE.Length)

try {
    $ramGb = [math]::Round((Get-CimInstance Win32_ComputerSystem).TotalPhysicalMemory / 1GB)
    Say ('Memory: ' + $ramGb + ' GB')
} catch { }
try {
    $letter = $env:LOCALAPPDATA.Substring(0, 1)
    $freeGb = [math]::Round((Get-PSDrive -Name $letter).Free / 1GB)
    Say ('Free space on drive ' + $letter + ': ' + $freeGb + ' GB')
} catch { }
try {
    foreach ($card in @(Get-CimInstance Win32_VideoController)) {
        Say ('Graphics: ' + $card.Name + ', driver ' + $card.DriverVersion)
    }
} catch { }
foreach ($line in (Nvidia-Line)) {
    Say ('nvidia-smi (name, total, used, driver): ' + $line)
}

$git = Tool-Version 'git' '--version'
$winget = Tool-Version 'winget' '--version'
Say ('git: ' + $(if ($git) { $git } else { 'not found' }))
Say ('winget: ' + $(if ($winget) { $winget } else { 'not found' }))
Say ('curl.exe: ' + $(if (Get-Command 'curl.exe' -ErrorAction SilentlyContinue) { 'found' } else { 'not found' }))
Say ('tar.exe: ' + $(if (Get-Command 'tar.exe' -ErrorAction SilentlyContinue) { 'found' } else { 'not found' }))

$condaRoot = $null
foreach ($base in @($env:USERPROFILE, $env:LOCALAPPDATA, $env:ProgramData)) {
    if (-not $base) { continue }
    foreach ($name in @('miniconda3', 'anaconda3', 'miniforge3', 'mambaforge')) {
        $root = Join-Path $base $name
        if (-not $condaRoot -and (Test-Path (Join-Path $root 'Scripts\conda.exe'))) { $condaRoot = $root }
    }
}
if ($condaRoot) {
    $envPython = Test-Path (Join-Path $condaRoot 'envs\sapphire\python.exe')
    Say ('conda: found at ' + $condaRoot + '; the sapphire environment is there = ' + $envPython)
} else {
    Say ('conda: not found in the usual folders' + $(if (Get-Command 'conda' -ErrorAction SilentlyContinue) { ' (but it is on PATH)' } else { '' }))
}

try {
    $vc = Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\VisualStudio\14.0\VC\Runtimes\x64' -ErrorAction Stop
    Say ('Visual C++ runtime: installed = ' + ($vc.Installed -eq 1) + ', version ' + $vc.Version)
} catch { Say 'Visual C++ runtime: not found (the local LLM needs it; the launcher installs it)' }

# ---- 2. the launcher, looking only ----------------------------------------------

Section 'The launcher'
try {
    $file = Get-Item $ExePath
    $hash = (Get-FileHash $ExePath -Algorithm SHA256).Hash.Substring(0, 16)
    Say ('File: ' + $file.Name + ', ' + [math]::Round($file.Length / 1MB, 1) + ' MB, sha256 starts ' + $hash)
} catch { }

# Status, three times, through a pipe.
$times = @()
$status = $null
$first = $null
for ($i = 0; $i -lt 3; $i++) {
    $r = Run-Launcher '--llm-status' 30
    if ($i -eq 0) { $first = $r }
    if ($r.Exited -and $r.Json) { $times += $r.Ms; $status = $r.Json }
}
if (-not $first.Exited) {
    Check 'FAIL' 'Status call returns' 'it was still running after 30 seconds, so it was ended'
} elseif ($first.PipeHeld) {
    Check 'FAIL' 'Status call returns' 'the launcher ended but its output never closed'
} elseif (-not $status) {
    Check 'FAIL' 'Status call returns JSON' ('exit code ' + $first.Code + ', it printed: ' + $first.Out)
} else {
    Check 'PASS' 'Status call returns JSON' ('launcher version ' + $status.version)
    $sorted = @($times | Sort-Object)
    $median = $sorted[[int][math]::Floor($sorted.Count / 2)]
    $verdict = 'PASS'
    if ($median -gt 1500) { $verdict = 'INFO' }
    Check $verdict 'Status call is quick' ('times in ms: ' + ($times -join ', '))

    $wanted = @('available', 'job', 'last_job', 'models', 'ok', 'phase', 'running', 'sapphire_running', 'set_up', 'settings', 'sign_in', 'state', 'version')
    $have = @($status.PSObject.Properties.Name)
    $missing = @($wanted | Where-Object { $have -notcontains $_ })
    if ($missing.Count -eq 0) { Check 'PASS' 'Status has every field Sapphire reads' '' } else { Check 'FAIL' 'Status has every field Sapphire reads' ('missing: ' + ($missing -join ', ')) }

    $modelName = '(none)'
    if ($status.settings.model) { $modelName = Split-Path -Leaf $status.settings.model }
    Say ('  phase = ' + $status.phase + ', state = ' + $status.state + ', set up = ' + $status.set_up + ', Sapphire running = ' + $status.sapphire_running + ', at sign-in = ' + $status.sign_in)
    Say ('  model = ' + $modelName + ', on = ' + $status.settings.on + ', only with Sapphire = ' + $status.settings.with_sapphire + ', graphics card = ' + $status.settings.gpu + ', context = ' + $status.settings.ctx + ', port = ' + $status.settings.port)
    $modelsDir = [string]$status.settings.models_dir
    $where = 'the default'
    if ($modelsDir -and -not $modelsDir.ToLower().StartsWith($env:LOCALAPPDATA.ToLower())) {
        $where = 'one you chose, on drive ' + $modelsDir.Substring(0, 1)
        Hide-Folder $modelsDir '<models folder>'
    }
    Say ('  models on disk = ' + @($status.models).Count + ', models folder = ' + $where)
    if ($status.job) { Say ('  a download is running: ' + $status.job.label + ', ' + $status.job.percent + '%') }
    if ($status.last_job) { Say ('  last download: ' + $status.last_job.state + $(if ($status.last_job.error) { ' (' + $status.last_job.error + ')' } else { '' })) }
}

# A switch nobody knows must be refused, never open a window.
$r = Run-Launcher '--no-such-switch-test' 15
if (-not $r.Exited) {
    Check 'FAIL' 'An unknown switch is refused' 'the launcher stayed open (probably a window), so it was ended. Launchers before 0.9.9 do this.'
} elseif ($r.Code -eq 1 -and $r.Json -and $r.Json.ok -eq $false) {
    Check 'PASS' 'An unknown switch is refused' ''
} else {
    Check 'FAIL' 'An unknown switch is refused' ('exit code ' + $r.Code + ', it printed: ' + $r.Out)
}

# The sign-in entry, read straight from the registry, against what the launcher says.
$runValue = $null
try { $runValue = (Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name 'SapphireLLM' -ErrorAction Stop).SapphireLLM } catch { }
$entry = 'off'
if ($runValue) {
    if ($runValue -match '--llm-watch') { $entry = 'watch' } elseif ($runValue -match '--start-llm') { $entry = 'start' }
}
if ($status) {
    if ($entry -eq $status.sign_in) {
        Check 'PASS' 'Sign-in entry matches what the launcher reports' ('both say ' + $entry)
    } else {
        Check 'FAIL' 'Sign-in entry matches what the launcher reports' ('registry says ' + $entry + ', launcher says ' + $status.sign_in)
    }
}
if ($runValue) {
    $pointsHere = $runValue.ToLower().Contains($ExePath.ToLower())
    Say ('  the sign-in entry points at this launcher = ' + $pointsHere)
}

# The Start menu shortcut.
$lnk = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Sapphire Launcher.lnk'
if (Test-Path $lnk) {
    $target = ''
    try { $target = (New-Object -ComObject WScript.Shell).CreateShortcut($lnk).TargetPath } catch { }
    if ($target -and (Test-Path $target)) {
        Check 'PASS' 'Start menu shortcut' ('there, and it opens a launcher that exists; this one = ' + ($target -ieq $ExePath))
    } else {
        $leaf = '(nothing)'
        if ($target) { $leaf = Split-Path -Leaf $target }
        Check 'FAIL' 'Start menu shortcut' ('there, but the file it points at is missing: ' + $leaf)
    }
} else {
    Check 'INFO' 'Start menu shortcut' 'not there yet (it is made the first time the launcher window opens)'
}

# Launcher processes: how many windows, watchers and download workers.
try {
    $procs = @(Get-CimInstance Win32_Process -Filter "Name = 'sapphire-launcher.exe'")
    $watchers = @($procs | Where-Object { $_.CommandLine -match '--llm-watch' }).Count
    $workers = @($procs | Where-Object { $_.CommandLine -match '--llm-job' }).Count
    $others = $procs.Count - $watchers - $workers
    $verdict = 'PASS'
    if ($watchers -gt 1 -or $workers -gt 1) { $verdict = 'FAIL' }
    Check $verdict 'At most one watcher and one download worker' ('watchers = ' + $watchers + ', download workers = ' + $workers + ', windows = ' + $others)
    $servers = @(Get-Process -Name 'llama-server' -ErrorAction SilentlyContinue).Count
    Say ('  llama-server processes running = ' + $servers)
} catch { Check 'INFO' 'Launcher processes' 'could not be listed' }

# Leftovers: half-written settings files and unfinished downloads.
$configDir = Join-Path $env:APPDATA 'Sapphire'
if (Test-Path $configDir) {
    $old = (Get-Date).AddMinutes(-2)
    $stray = @(Get-ChildItem $configDir -File -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '\.tmp' -and $_.LastWriteTime -lt $old })
    if ($stray.Count -eq 0) { Check 'PASS' 'No half-written settings files left behind' '' } else { Check 'FAIL' 'No half-written settings files left behind' (($stray | ForEach-Object { $_.Name }) -join ', ') }
} else {
    Check 'INFO' 'Settings folder' 'not there: the launcher has not saved anything yet'
}
if ($status -and $status.settings.models_dir -and (Test-Path $status.settings.models_dir)) {
    $parts = @(Get-ChildItem $status.settings.models_dir -File -Filter '*.part' -ErrorAction SilentlyContinue)
    if ($parts.Count -gt 0 -and -not $status.job) {
        Check 'INFO' 'Unfinished downloads' (($parts | ForEach-Object { $_.Name + ' (' + [math]::Round($_.Length / 1MB) + ' MB)' }) -join ', ')
    }
}

# ---- 3. the LLM, started and stopped (asks first) -------------------------------

Section 'Starting and stopping the local LLM'
$reason = ''
if ($ReadOnly) {
    $reason = 'left out (-ReadOnly)'
} elseif (-not $status) {
    $reason = 'the status call did not work'
} elseif (-not $status.set_up) {
    $reason = 'no model is set up yet: pick one on the Install tab, then run this again'
} elseif ($status.sapphire_running) {
    $reason = 'Sapphire is running: stop her first, then run this again'
} elseif ($status.phase -ne 'stopped') {
    $reason = 'the LLM is ' + $status.phase + ' right now, and this check will not disturb it'
} elseif (-not $Active) {
    Write-Host ''
    Write-Host 'The next part starts the local LLM, checks it, and stops it again (about a minute).'
    if (-not (Ask-YesNo 'Run it?')) { $reason = 'you said no' }
}

if ($reason) {
    Check 'SKIP' 'LLM start and stop' $reason
} else {
    $before = Nvidia-Line
    # Core's contract: the call returns in seconds with JSON while the server keeps running.
    $r = Run-Launcher '--start-llm' 60
    $started = $false
    if (-not $r.Exited) {
        Check 'FAIL' 'Start returns while the LLM keeps running' 'the call was still running after 60 seconds, so it was ended'
    } elseif ($r.PipeHeld) {
        Check 'FAIL' 'Start returns while the LLM keeps running' 'the launcher ended, but the server it started holds the output pipe: Sapphire would hang on this call'
        $started = $true
    } elseif ($r.Json -and $r.Json.ok) {
        Check 'PASS' 'Start returns while the LLM keeps running' ('in ' + $r.Ms + ' ms')
        $started = $true
    } else {
        Check 'FAIL' 'Start returns while the LLM keeps running' ('exit code ' + $r.Code + ', it printed: ' + $r.Out)
    }

    if ($started) {
        # Wait for it to answer.
        $watch = [System.Diagnostics.Stopwatch]::StartNew()
        $now = $null
        while ($watch.Elapsed.TotalSeconds -lt 240) {
            $now = Get-Status
            if ($now -and ($now.phase -eq 'running' -or $now.phase -eq 'stopped' -or $now.phase -eq 'none')) { break }
            Start-Sleep -Seconds 2
        }
        if ($now -and $now.phase -eq 'running') {
            Check 'PASS' 'The model loads' ('ready after ' + [int]$watch.Elapsed.TotalSeconds + ' seconds')
            $serverPid = [int]$now.running.pid
            $proc = Get-Process -Id $serverPid -ErrorAction SilentlyContinue
            if ($proc -and $proc.ProcessName -eq 'llama-server') { Check 'PASS' 'The process it names is llama-server' '' } else { Check 'FAIL' 'The process it names is llama-server' ('pid ' + $serverPid + ' is ' + $(if ($proc) { $proc.ProcessName } else { 'not running' })) }
            $code = Health-Code ([int]$status.settings.port)
            if ($code -eq 200) { Check 'PASS' 'The LLM answers on its port' '' } else { Check 'FAIL' 'The LLM answers on its port' ('answer code ' + $code) }
            foreach ($line in (Nvidia-Line)) { Say ('  nvidia-smi with the model loaded: ' + $line) }
            foreach ($line in $before) { Say ('  nvidia-smi before: ' + $line) }
        } else {
            $phase = '(no status)'
            if ($now) { $phase = $now.phase }
            Check 'FAIL' 'The model loads' ('after ' + [int]$watch.Elapsed.TotalSeconds + ' seconds the phase is ' + $phase)
            $log = Join-Path $configDir 'llm.log'
            if (Test-Path $log) {
                $bad = @(Get-Content $log -Tail 60 -ErrorAction SilentlyContinue | Where-Object { $_ -match 'error|fail|unable|cannot|invalid' } | Select-Object -Last 6)
                foreach ($line in $bad) { Say ('  log: ' + $line) }
            }
        }

        # Stop, and make sure it is really gone.
        $serverPid = 0
        if ($now -and $now.running) { $serverPid = [int]$now.running.pid }
        $r = Run-Launcher '--stop-llm' 60
        $gone = $false
        for ($i = 0; $i -lt 20; $i++) {
            $after = Get-Status
            $alive = $false
            if ($serverPid -gt 0) { $alive = [bool](Get-Process -Id $serverPid -ErrorAction SilentlyContinue) }
            if ($after -and $after.phase -eq 'stopped' -and -not $alive) { $gone = $true; break }
            Start-Sleep -Seconds 1
        }
        if ($r.Exited -and $r.Json -and $r.Json.ok -and $gone) {
            Check 'PASS' 'Stop ends the LLM' ''
        } else {
            Check 'FAIL' 'Stop ends the LLM' ('stop call exit code ' + $r.Code + ', gone = ' + $gone + '. If llama-server is still listed in Task Manager, end it there.')
        }
    }

    # The same model through a folder with an accented letter in its name, on the
    # processor only. No copy is made: a second name for the same file (a hard link).
    $llmJson = Join-Path $configDir 'llm.json'
    $engine = $null
    try { $engine = (Get-Content $llmJson -Raw | ConvertFrom-Json).engine } catch { }
    $model = $status.settings.model
    if ($engine -and $model -and (Test-Path $engine) -and (Test-Path $model)) {
        $oddDir = Join-Path (Split-Path -Parent $model) ('sl-t' + [char]0xE9 + 'st')
        $oddModel = Join-Path $oddDir 'm.gguf'
        $errFile = Join-Path $env:TEMP ('sl-test-err-' + $PID + '.txt')
        $outFile = Join-Path $env:TEMP ('sl-test-out-' + $PID + '.txt')
        $server = $null
        try {
            New-Item -ItemType Directory -Path $oddDir -Force | Out-Null
            if (Test-Path $oddModel) { Remove-Item $oddModel -Force }
            New-Item -ItemType HardLink -Path $oddModel -Value $model -ErrorAction Stop | Out-Null
            $port = Free-Port
            $arguments = '-m "' + $oddModel + '" --host 127.0.0.1 --port ' + $port + ' --device none -c 2048'
            $server = Start-Process -FilePath $engine -ArgumentList $arguments -WorkingDirectory (Split-Path -Parent $engine) -WindowStyle Hidden -PassThru -RedirectStandardError $errFile -RedirectStandardOutput $outFile
            $null = $server.Handle   # without this, ExitCode can come back empty
            $verdict = ''
            for ($i = 0; $i -lt 45; $i++) {
                Start-Sleep -Seconds 1
                if ($server.HasExited) { $verdict = 'exited'; break }
                $code = Health-Code $port
                if ($code -eq 200) { $verdict = 'loaded'; break }
                if ($code -eq 503 -and $i -ge 12) { $verdict = 'loading'; break }
            }
            if ($verdict -eq 'loaded' -or $verdict -eq 'loading') {
                Check 'PASS' 'llama-server opens a model in a folder with an accented letter' $verdict
            } elseif ($verdict -eq 'exited') {
                Check 'FAIL' 'llama-server opens a model in a folder with an accented letter' ('it stopped with code ' + $server.ExitCode)
                $bad = @(Get-Content $errFile, $outFile -ErrorAction SilentlyContinue | Where-Object { $_ -match 'error|fail|unable|cannot|invalid' } | Select-Object -Last 5)
                foreach ($line in $bad) { Say ('  it said: ' + $line) }
            } else {
                Check 'INFO' 'llama-server opens a model in a folder with an accented letter' 'no answer in 45 seconds, but it had not stopped either'
            }
        } catch {
            Check 'SKIP' 'llama-server opens a model in a folder with an accented letter' ('could not set it up: ' + $_.Exception.Message)
        } finally {
            # Only the server this script started, then the extra name and its folder.
            if ($server -and -not $server.HasExited) { try { $server.Kill(); $server.WaitForExit(10000) | Out-Null } catch { } }
            Remove-Item $oddModel -Force -ErrorAction SilentlyContinue
            Remove-Item $oddDir -Force -ErrorAction SilentlyContinue
            Remove-Item $errFile, $outFile -Force -ErrorAction SilentlyContinue
        }
    } else {
        Check 'SKIP' 'llama-server opens a model in a folder with an accented letter' 'the engine or the model was not found'
    }
}

# ---- 4. what only a person can see ----------------------------------------------

if (-not $NoQuestions) {
    Section 'What you saw yourself'
    Write-Host 'Answer y if it worked, n if it did not, or just press Enter if you did not try it.'
    $questions = @(
        'Picking a model on the Install tab downloaded it and showed a percent bar',
        'Closing the launcher during a download and opening it again showed the download still going or finished',
        'Launch started Sapphire, and Open Browser said "Starting..." until she was ready',
        'Stop in the launcher stopped Sapphire, and the launcher itself stayed open',
        'With "Only run with Sapphire" ticked, the LLM came up with her and went away with her',
        'Sapphire started by hand in a terminal, then Stop in the launcher: she stopped and the launcher stayed open',
        '"Sapphire Launcher" is in the Start menu and opens the launcher',
        'Every Windows permission prompt was announced by the launcher before it appeared',
        'No black console window flashed up at any point'
    )
    foreach ($q in $questions) {
        $answer = Read-Host ('  ' + $q + ' [y/n/Enter]')
        $mark = 'not tried'
        if ($answer -match '^(y|yes)$') { $mark = 'yes' } elseif ($answer -match '^(n|no)$') { $mark = 'NO' }
        $Lines.Add('[' + $mark + '] ' + $q)
    }
    $note = Read-Host '  Anything odd? One line, or Enter to skip'
    if ($note) { $Lines.Add('Note: ' + (Redact $note)) }
}

# ---- the report -----------------------------------------------------------------

Say ''
Say ('Summary: ' + $Counts.PASS + ' passed, ' + $Counts.FAIL + ' failed, ' + $Counts.SKIP + ' skipped, ' + $Counts.INFO + ' notes.')

$reportPath = Join-Path $Here 'sapphire-launcher-test-report.txt'
try {
    Set-Content -Path $reportPath -Value $Lines -Encoding UTF8 -ErrorAction Stop
} catch {
    $reportPath = Join-Path ([Environment]::GetFolderPath('Desktop')) 'sapphire-launcher-test-report.txt'
    Set-Content -Path $reportPath -Value $Lines -Encoding UTF8
}
try { Set-Clipboard -Value ($Lines -join "`r`n") } catch { }
Write-Host ''
Write-Host ('The report is saved here: ' + $reportPath)
Write-Host 'It is also on your clipboard. Read it over, then paste it where the testers report.'
if ($Counts.FAIL -gt 0) { exit 1 }
exit 0
