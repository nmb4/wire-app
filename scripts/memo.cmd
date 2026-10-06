@echo off
rem Project-local OptMem entry point (cmd.exe, PowerShell).
rem Runs the vendored tool (.optmem\memo) with a Python 3 found on this machine
rem and keeps the store in .optmem\memory. Nothing is installed user-wide:
rem no %USERPROFILE%\.optmem, no PATH change, no persistent environment vars.
rem Git Bash / macOS / Linux: use scripts\memo instead.
rem
rem usage: scripts\memo.cmd ^<init^|wake^|note^|nap^|recall^|zoom^|forget^|config^|import^> [args...]
rem docs:  https://github.com/VictorTaelin/OptMem
setlocal
for %%I in ("%~dp0..") do set "ROOT=%%~fI"
set "TOOL=%ROOT%\.optmem\memo"
if not defined MEMORY_DIR set "MEMORY_DIR=%ROOT%\.optmem\memory"

if not exist "%TOOL%" (
  echo scripts\memo.cmd: missing %TOOL% ^(vendored OptMem tool not installed^) 1>&2
  exit /b 1
)

rem Prefer the py launcher: bare python/python3 may be a Microsoft Store alias.
rem Each probe exits 0 only for an actual Python 3.
where py >nul 2>nul && py -3 -c "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)" >nul 2>nul && goto :run_py
where python3 >nul 2>nul && python3 -c "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)" >nul 2>nul && goto :run_py3
where python >nul 2>nul && python -c "import sys; sys.exit(0 if sys.version_info[0] == 3 else 1)" >nul 2>nul && goto :run_python

echo scripts\memo.cmd: OptMem needs Python 3, but none was found. 1>&2
echo Install it from https://www.python.org/downloads/ and run again. 1>&2
exit /b 1

:run_py
py -3 "%TOOL%" %*
exit /b %errorlevel%

:run_py3
python3 "%TOOL%" %*
exit /b %errorlevel%

:run_python
python "%TOOL%" %*
exit /b %errorlevel%
