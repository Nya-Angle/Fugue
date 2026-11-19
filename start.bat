@echo off
cd /d "%~dp0"
title Fugue

echo [Fugue] Building and launching...
cargo run %*
if errorlevel 1 goto :fail
exit /b 0

:fail
echo.
echo [Fugue] Failed to start (exit code: %ERRORLEVEL%).
pause
exit /b 1
