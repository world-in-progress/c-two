param(
    [Parameter(Mandatory)][string]$PythonExecutable,
    [Parameter(Mandatory)][string]$UvExecutable,
    [Parameter(Mandatory)][string]$Helper,
    [Parameter(Mandatory)][string]$FastdbWheel,
    [Parameter(Mandatory)][string]$CTwoWheel,
    [Parameter(Mandatory)][string]$C3,
    [Parameter(Mandatory)][string]$Receipt,
    [ValidateRange(30, 900)][int]$TimeoutSeconds = 900
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$Receipt = [IO.Path]::GetFullPath($Receipt)
$report = [ordered]@{
    schema = 'c-two.standard-user-wrapper.v1'
    status = 'failed'
    helper_receipt = [IO.Path]::GetFileName($Receipt)
    cleanup = [ordered]@{}
    errors = [Collections.Generic.List[string]]::new()
}
$testAccount = $testSid = $credential = $secret = $child = $workspace = $accountName = $null
$workspaceCreated = $false
$consumerPassed = $false

function Get-InputFile([string]$Value, [switch]$Wheel) {
    $item = Get-Item -LiteralPath $Value
    if ($Wheel -and $item.PSIsContainer) {
        $files = @(Get-ChildItem -LiteralPath $item.FullName -Filter '*.whl' -File)
        if ($files.Count -ne 1) { throw "Expected one wheel in $Value" }
        $item = $files[0]
    }
    if ($item.PSIsContainer) { throw "Expected a file: $Value" }
    return $item.FullName
}

function Add-Diagnostic([string]$Message) {
    # Account names are not needed in public diagnostics. The random password
    # is never passed to a command, child environment, transcript, or file.
    if ($accountName) { $Message = $Message.Replace($accountName, '<ephemeral-user>') }
    $report.errors.Add($Message)
}

function Add-Failure([System.Management.Automation.ErrorRecord]$Record) {
    # Keep the failing script line: hosted-only failures otherwise need another
    # full CI cycle before the cause is visible.
    $message = $Record.Exception.Message
    if ($Record.InvocationInfo) { $message = "line $($Record.InvocationInfo.ScriptLineNumber): $message" }
    Add-Diagnostic $message
}

function Get-OwnedProcesses {
    $owned = [Collections.Generic.List[object]]::new()
    $deadline = [DateTime]::UtcNow.AddSeconds(30)
    if ($testSid) {
        foreach ($process in Get-CimInstance Win32_Process -OperationTimeoutSec 10) {
            if ([DateTime]::UtcNow -gt $deadline) { throw 'Owned-process inspection exceeded its time budget.' }
            # Protected system processes can refuse inspection. Only an exact
            # SID match authorizes cleanup; names or PID ancestry are not enough.
            $owner = Invoke-CimMethod -InputObject $process -MethodName GetOwnerSid -OperationTimeoutSec 5 -ErrorAction SilentlyContinue
            if ($owner -and $owner.ReturnValue -eq 0 -and $owner.Sid -eq $testSid.Value) {
                $owned.Add($process)
            }
        }
    }
    return $owned.ToArray()
}

try {
    if (-not $IsWindows -or $env:GITHUB_ACTIONS -ne 'true' -or $env:RUNNER_ENVIRONMENT -ne 'github-hosted') {
        throw 'This wrapper is restricted to disposable GitHub-hosted Windows runners.'
    }
    $parentIdentity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $parentPrincipal = [Security.Principal.WindowsPrincipal]::new($parentIdentity)
    if (-not $parentPrincipal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'The parent runner must be able to create and remove its ephemeral local account.'
    }
    $pythonPath = Get-InputFile $PythonExecutable
    $uvPath = Get-InputFile $UvExecutable
    $helperPath = Get-InputFile $Helper
    $fastdbPath = Get-InputFile $FastdbWheel -Wheel
    $ctwoPath = Get-InputFile $CTwoWheel -Wheel
    $cliPath = Get-InputFile $C3
    New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($Receipt)) | Out-Null

    $accountName = 'c2w' + [Guid]::NewGuid().ToString('N').Substring(0, 13)
    $randomBytes = [Security.Cryptography.RandomNumberGenerator]::GetBytes(32)
    $passwordText = 'Aa1!' + [Convert]::ToBase64String($randomBytes)
    $secret = ConvertTo-SecureString -String $passwordText -AsPlainText -Force
    [Array]::Clear($randomBytes, 0, $randomBytes.Length)
    $passwordText = $null
    $testAccount = New-LocalUser -Name $accountName -Password $secret -AccountExpires (Get-Date).AddHours(2) -UserMayNotChangePassword
    $testSid = $testAccount.SID
    $usersSid = [Security.Principal.SecurityIdentifier]::new('S-1-5-32-545')
    if (-not ((Get-LocalGroupMember -SID $usersSid) | Where-Object { $_.SID -eq $testSid })) {
        Add-LocalGroupMember -SID $usersSid -Member $testAccount
    }
    $report.account_sid = $testSid.Value
    $credential = [Management.Automation.PSCredential]::new("$env:COMPUTERNAME\$accountName", $secret)

    $workspace = Join-Path $env:PUBLIC ('c-two-standard-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $workspace | Out-Null
    $workspaceCreated = $true
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    $acl.SetOwner($parentIdentity.User)
    foreach ($sid in @($parentIdentity.User, [Security.Principal.SecurityIdentifier]::new('S-1-5-18'), $testSid)) {
        $acl.AddAccessRule([Security.AccessControl.FileSystemAccessRule]::new(
            $sid, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow'))
    }
    Set-Acl -LiteralPath $workspace -AclObject $acl
    foreach ($directory in @('tools', 'artifacts', 'temp', 'profile', 'profile/AppData/Local', 'profile/AppData/Roaming')) {
        New-Item -ItemType Directory -Force -Path (Join-Path $workspace $directory) | Out-Null
    }
    $staged = @{}
    foreach ($entry in @(
            @('helper', $helperPath, 'tools/windows_wheel_smoke.py'),
            @('uv', $uvPath, 'tools/uv.exe'),
            @('fastdb', $fastdbPath, ('artifacts/' + [IO.Path]::GetFileName($fastdbPath))),
            @('ctwo', $ctwoPath, ('artifacts/' + [IO.Path]::GetFileName($ctwoPath))),
            @('cli', $cliPath, 'artifacts/c3.exe'))) {
        $destination = Join-Path $workspace $entry[2]
        Copy-Item -LiteralPath $entry[1] -Destination $destination
        if ((Get-FileHash -LiteralPath $entry[1]).Hash -ne (Get-FileHash -LiteralPath $destination).Hash) {
            throw "Staged $($entry[0]) hash differs from the supplied artifact."
        }
        $staged[$entry[0]] = $destination
    }
    $config = @{ workspace = $workspace; staged = $staged; receipt = (Join-Path $workspace 'consumer-receipt.json') }
    $configPath = Join-Path $workspace 'inputs.json'
    $config | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $configPath -Encoding utf8
    $driverPath = Join-Path $workspace 'driver.py'
    @'
import json, os, pathlib, runpy, sys
config = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8-sig"))
root = pathlib.Path(config["workspace"])
system = os.environ["SystemRoot"]
environment = {name: os.environ[name] for name in
               ("SystemRoot", "WINDIR", "ComSpec", "OS", "PROCESSOR_ARCHITECTURE", "NUMBER_OF_PROCESSORS")
               if name in os.environ}
environment.update({
    "PATH": os.pathsep.join((str(root / "tools"), str(pathlib.Path(sys.executable).parent),
                             str(pathlib.Path(system) / "System32"), system)),
    "PATHEXT": ".COM;.EXE;.BAT;.CMD", "PYTHONUTF8": "1",
    "TEMP": str(root / "temp"), "TMP": str(root / "temp"),
    "USERPROFILE": str(root / "profile"), "HOME": str(root / "profile"),
    "LOCALAPPDATA": str(root / "profile/AppData/Local"),
    "APPDATA": str(root / "profile/AppData/Roaming"),
})
os.environ.clear()
os.environ.update(environment)
staged = config["staged"]
sys.argv = [staged["helper"], "--fastdb-wheel", staged["fastdb"],
            "--c-two-wheel", staged["ctwo"], "--c3", staged["cli"],
            "--receipt", config["receipt"], "--require-standard-user"]
runpy.run_path(staged["helper"], run_name="__main__")
'@ | Set-Content -LiteralPath $driverPath -Encoding utf8

    $arguments = '-X utf8 -I "{0}" "{1}"' -f $driverPath, $configPath
    $child = Start-Process -FilePath $pythonPath -ArgumentList $arguments -Credential $credential `
        -WorkingDirectory $workspace -PassThru -LoadUserProfile:$false `
        -RedirectStandardOutput (Join-Path $workspace 'stdout.log') `
        -RedirectStandardError (Join-Path $workspace 'stderr.log')
    $report.child_pid = $child.Id
    if (-not $child.WaitForExit($TimeoutSeconds * 1000)) { throw 'Standard-user consumer exceeded its bounded timeout.' }
    $report.child_exit_code = $child.ExitCode
    if ($child.ExitCode -ne 0) { throw "Standard-user consumer failed with exit code $($child.ExitCode)." }
    $consumer = Get-Content -LiteralPath $config.receipt -Raw | ConvertFrom-Json
    if ($consumer.status -ne 'passed' -or $consumer.identity.administrator -ne $false) {
        throw 'The consumer did not prove a passing Windows token without administrator membership.'
    }
    $consumerPassed = $true
} catch {
    Add-Failure $_
} finally {
    if ($testSid) {
        try {
            $reaped = [Collections.Generic.HashSet[uint32]]::new()
            $reapDeadline = [DateTime]::UtcNow.AddSeconds(60)
            for ($attempt = 0; $attempt -lt 3; $attempt++) {
                if ([DateTime]::UtcNow -gt $reapDeadline) { break }
                $owned = @(Get-OwnedProcesses)
                if ($owned.Count -eq 0) { break }
                foreach ($process in $owned) {
                    if ([DateTime]::UtcNow -gt $reapDeadline) { break }
                    $reaped.Add($process.ProcessId) | Out-Null
                    # Exiting a launcher may also reap its child. Inspect again
                    # after termination rather than treating that race as a leak.
                    Invoke-CimMethod -InputObject $process -MethodName Terminate -Arguments @{ Reason = [uint32]1 } `
                        -OperationTimeoutSec 5 -ErrorAction SilentlyContinue | Out-Null
                }
                Start-Sleep -Milliseconds 100
            }
            $report.cleanup.reaped_pids = @($reaped)
            if ($child -and -not $child.WaitForExit(10000)) { throw 'Consumer process did not exit during cleanup.' }
            $remaining = @(Get-OwnedProcesses)
            $report.cleanup.remaining_pids = @($remaining | ForEach-Object { $_.ProcessId })
            $report.cleanup.processes_exited = $remaining.Count -eq 0
            if ($remaining.Count -ne 0) { throw 'Owned processes remain after cleanup.' }
        } catch { Add-Failure $_ }
    }
    if ($workspaceCreated) {
        try {
            foreach ($entry in @(@('consumer-receipt.json', $Receipt), @('stdout.log', "$Receipt.stdout.log"), @('stderr.log', "$Receipt.stderr.log"))) {
                $source = Join-Path $workspace $entry[0]
                if (Test-Path -LiteralPath $source -PathType Leaf) {
                    if ($entry[0] -eq 'consumer-receipt.json') {
                        Copy-Item -LiteralPath $source -Destination $entry[1]
                    } else {
                        $content = [string](Get-Content -LiteralPath $source -Raw)
                        # Start-Process creates both redirect files eagerly, so an
                        # empty stderr log is normal. Get-Content -Raw then yields
                        # AutomationNull, which the [string] cast keeps as $null
                        # instead of the empty string Replace requires.
                        if ($null -eq $content) { $content = '' }
                        $content.Replace($accountName, '<ephemeral-user>') | Set-Content -LiteralPath $entry[1] -Encoding utf8 -NoNewline
                    }
                }
            }
        } catch { Add-Failure $_ }
        try {
            Remove-Item -LiteralPath $workspace -Recurse -Force
            $report.cleanup.workspace_removed = -not (Test-Path -LiteralPath $workspace)
        } catch { Add-Failure $_ }
    }
    if ($testAccount) {
        try {
            Remove-LocalUser -SID $testSid -Confirm:$false
            $report.cleanup.account_removed = @((Get-LocalUser) | Where-Object { $_.SID -eq $testSid }).Count -eq 0
        } catch { Add-Failure $_ }
    }
    if ($child) { $child.Dispose() }
    if ($secret) { $secret.Dispose() }
    if ($consumerPassed -and $report.errors.Count -eq 0 -and $report.cleanup.processes_exited -and
        $report.cleanup.workspace_removed -and $report.cleanup.account_removed) {
        $report.status = 'passed'
    }
    New-Item -ItemType Directory -Force -Path ([IO.Path]::GetDirectoryName($Receipt)) | Out-Null
    $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath "$Receipt.wrapper.json" -Encoding utf8
}
Write-Output "Standard-user wheel consumer: $($report.status)"
if ($report.status -ne 'passed') { exit 1 }
