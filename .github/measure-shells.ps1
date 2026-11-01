# TEMPORARY measurement (drop before merge): exit code, stdin bytes and
# stderr bytes through Windows OpenSSH under cmd.exe, powershell.exe 5.1 and
# pwsh 7 login shells.
$ErrorActionPreference = 'Continue'
$h = $env:TENDR_TEST_WIN_SSH_HOST
$s = $env:TENDR_TEST_WIN_SSH_SCRATCH
$bin = 'C:\tendr-ssh-bin'
$tendr = (Resolve-Path target\debug\tendr.exe).Path
$tmp = $env:RUNNER_TEMP
$utf8 = New-Object System.Text.UTF8Encoding($false)

function Esc([byte[]]$b) {
  if ($null -eq $b) { return '<none>' }
  $t = $utf8.GetString($b)
  ($t -replace "`r", '\r' -replace "`n", '\n')
}
function Same([byte[]]$a, [byte[]]$b) {
  if ($null -eq $a -or $null -eq $b) { return $false }
  [System.Linq.Enumerable]::SequenceEqual($a, $b)
}
function ReadShared([string]$path) {
  $fs = [IO.File]::Open($path, 'Open', 'Read', 'ReadWrite')
  $ms = New-Object IO.MemoryStream; $fs.CopyTo($ms); $fs.Close(); , $ms.ToArray()
}
function Procs([string]$why) {
  "--- processes ($why):"
  Get-CimInstance Win32_Process | Where-Object { $_.Name -match '^(tendr|ssh|sshd|sshd-session|powershell|pwsh|cmd|conhost)\.exe$' } |
    ForEach-Object { "  $($_.ProcessId) <- $($_.ParentProcessId) $($_.Name): $($_.CommandLine)" }
}
# Run a command line under local cmd.exe with byte-exact redirects; 90 s cap.
# A hang lists the live processes, then kills the whole tree.
function RunCmd([string]$line, [string]$tag) {
  $out = "$tmp\$tag.out"; $err = "$tmp\$tag.err"
  $p = Start-Process cmd.exe -ArgumentList '/d', '/s', '/c', "`"$line > `"$out`" 2> `"$err`"`"" -NoNewWindow -PassThru
  $null = $p.Handle
  $sw = [Diagnostics.Stopwatch]::StartNew()
  if (-not $p.WaitForExit(90000)) {
    $code = 'HANG'
    Procs "HANG: $line" | Write-Host
    taskkill.exe /T /F /PID $p.Id | Out-Null
    Start-Sleep 2
  } else { $code = $p.ExitCode }
  [pscustomobject]@{ code = $code; secs = [math]::Round($sw.Elapsed.TotalSeconds, 1)
    out = (ReadShared $out); err = (ReadShared $err) }
}
function T([string]$line, [string]$tag) { RunCmd "`"$tendr`" --host $h $line" $tag }

# Remote byte probe: fake_ssh copies stdin to a file, writes fixed stderr and
# stdout, exits 42.
New-Item -ItemType Directory -Force "$bin\probe" | Out-Null
Copy-Item target\debug\fake_ssh.exe "$bin\probe\probe.exe" -Force
# On the PATH with a plain name, so `start` needs no quoting under cmd.exe.
Copy-Item target\debug\fake_ssh.exe "$bin\pushprobe.exe" -Force
[IO.File]::WriteAllText("$bin\probe\fake-ssh-actions",
  "stdin $s\probe-stdin.bin`nerr tendr-hello 1\nsecond é line\n`nout out é\n`nexit 42`n", $utf8)
$input_bytes = [byte[]](@(0..255) + $utf8.GetBytes("crlf`r`nend é 日本`n"))
[IO.File]::WriteAllBytes("$tmp\in.bin", $input_bytes)
$want_err = $utf8.GetBytes("tendr-hello 1`nsecond é line`n")
$want_out = $utf8.GetBytes("out é`n")

$shells = [ordered]@{ 'cmd.exe' = $null
  'powershell.exe 5.1' = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
  'pwsh 7' = 'C:\Program Files\PowerShell\7\pwsh.exe' }
$rows = @()
$n = 0
foreach ($name in $shells.Keys) {
  $n++
  $path = $shells[$name]
  if ($path -and -not (Test-Path $path)) { $rows += "| $name | not installed on this runner | | | | | |"; continue }
  if ($path) {
    New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value $path -PropertyType String -Force | Out-Null
  } else {
    Remove-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -ErrorAction SilentlyContinue
  }
  "===== $name"
  Remove-Item "$s\probe-stdin.bin" -ErrorAction SilentlyContinue

  # 1. Raw ssh: native exit code, stdin bytes, stderr/stdout bytes.
  $r = RunCmd "ssh -T $h C:\tendr-ssh-bin\probe\probe.exe < `"$tmp\in.bin`"" "raw$n"
  $got = if (Test-Path "$s\probe-stdin.bin") { [IO.File]::ReadAllBytes("$s\probe-stdin.bin") } else { $null }
  $rawExit = $r.code
  $rawStdin = if (Same $got $input_bytes) { 'exact' } elseif ($got) { "DIFF ($($got.Length) of $($input_bytes.Length) B)" } else { 'none' }
  $rawErr = if (Same $r.err $want_err) { 'exact' } else { "DIFF: $(Esc $r.err)" }
  $rawOut = if (Same $r.out $want_out) { 'exact' } else { "DIFF: $(Esc $r.out)" }
  "raw: exit=$rawExit secs=$($r.secs) stdin=$rawStdin stderr=$rawErr stdout=$rawOut"

  # 2. Real remote tendr stderr, direct over ssh.
  $r = RunCmd "ssh -T $h tendr --hello status nope$n" "hello$n"
  "tendr --hello stderr: exit=$($r.code) [$(Esc $r.err)]"
  $helloErr = Esc $r.err

  # 3. Through the local tendr: connect deadline path, wait exit code.
  $r = T "status nope$n" "status$n"
  $deadline = "exit=$($r.code) in $($r.secs)s, stderr [$(Esc $r.err)]"
  "status via tendr: $deadline"
  $null = T "start ec$n -- cmd /d /c exit 7" "ecs$n"
  $r = T "wait ec$n --timeout 60" "ecw$n"
  $waitExit = $r.code
  "wait (child exits 7): exit=$waitExit"

  # 4. exec (JSON frame on ssh stdin): non-ASCII and CRLF payload.
  $null = T "start ex$n --stdin --exec-target powershell -- powershell -NoProfile" "exs$n"
  for ($i = 0; $i -lt 60; $i++) { $st = T "status ex$n" "exst$n"; if ((Esc $st.out) -match '"status": "Running"') { break }; Start-Sleep 1 }
  # The frame exactly as main sends it, with raw CRLF between JSON tokens.
  $frame = "{`r`n`"v`":1,`"session`":`"ex$n`",`"cmd`":[`"Write-Output 'é-ü 日本'\r\nWrite-Output 'line2'`"],`"timeout`":30}`r`n"
  [IO.File]::WriteAllText("$tmp\frame$n.json", $frame, $utf8)
  $r = RunCmd "`"$tendr`" --host $h exec --frame-from-stdin < `"$tmp\frame$n.json`"" "exec$n"
  $exCode = $r.code
  $exText = Esc $r.out
  $exOk = ($exText -match 'é-ü 日本\\r\\nline2')
  "exec: exit=$exCode ok=$exOk in $($r.secs)s: stdout [$exText] stderr [$(Esc $r.err)]"
  if (-not $exOk) {
    "exec status after: $((Esc (T "status ex$n" "exsa$n").out) -replace '\s+', ' ')"
    $r = RunCmd "ssh -T $h tendr --hello exec --frame-from-stdin < `"$tmp\frame$n.json`"" "execraw$n"
    "exec direct over ssh: exit=$($r.code) in $($r.secs)s: stdout [$(Esc $r.out)] stderr [$(Esc $r.err)]"
    $r = RunCmd "`"$tendr`" --host $h exec ex$n --timeout 20 -- Write-Output plain" "execplain$n"
    "exec plain argv form: exit=$($r.code) in $($r.secs)s: stdout [$(Esc $r.out)] stderr [$(Esc $r.err)]"
  }
  $null = T "kill ex$n --force" "exk$n"

  # 5. push binary bytes to a child that copies its stdin to a file.
  $pushFile = "$s\push$n.bin"
  [IO.File]::WriteAllText("$bin\fake-ssh-actions", "stdin $pushFile`nsleep 600`n", $utf8)
  $null = T "start pu$n --stdin -- pushprobe" "pus$n"
  for ($i = 0; $i -lt 60; $i++) { $st = T "status pu$n" "pust$n"; if ((Esc $st.out) -match '"status": "Running"') { break }; Start-Sleep 1 }
  $r = RunCmd "`"$tendr`" --host $h push pu$n < `"$tmp\in.bin`"" "push$n"
  $pushExit = $r.code
  $pgot = $null
  for ($i = 0; $i -lt 30; $i++) {
    if (Test-Path $pushFile) {
      $pgot = ReadShared $pushFile
      if ($pgot.Length -ge $input_bytes.Length) { break }
    }
    Start-Sleep 1
  }
  $push = if (Same $pgot $input_bytes) { 'exact' } elseif ($pgot) { "DIFF ($($pgot.Length) of $($input_bytes.Length) B)" } else { 'none' }
  $after = Esc (T "status pu$n" "pusa$n").out
  "push: exit=$pushExit bytes=$push stderr [$(Esc $r.err)] status-after [$($after -replace '\s+', ' ')]"
  $null = T "kill pu$n --force" "puk$n"

  $rows += "| $name | $rawExit | $waitExit | $rawStdin | $push | $(if ($exOk) {'ok'} else {"FAIL exit=$exCode"}) | $rawErr | $deadline |"
}
Remove-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -ErrorAction SilentlyContinue
$table = @("### $env:RUNNER_OS $env:RUNNER_ARCH",
  '| login shell | raw ssh exit (native 42) | tendr wait exit (want 42) | raw ssh stdin bytes | tendr push bytes | tendr exec frame (é/日本/CRLF) | raw stderr bytes | tendr status nope (deadline path) |',
  '|---|---|---|---|---|---|---|---|') + $rows
$table -join "`n" | Tee-Object -Append $env:GITHUB_STEP_SUMMARY
