@echo off
REM Build and drop the exe somewhere findable.
REM Cargo always writes to target\release\, which is buried; this copies it up
REM into dist\ so there is one obvious place to grab it from.
setlocal
cd /d "%~dp0"

cargo build --release
if errorlevel 1 (
    echo BUILD FAILED
    exit /b 1
)

if not exist dist mkdir dist
copy /y "target\release\hidforge.exe" "dist\hidforge.exe" >nul
if errorlevel 1 (
    echo Could not copy - is HidForge still running?
    exit /b 1
)

echo.
echo Built:  %CD%\dist\hidforge.exe
echo.
echo   hidforge.exe             launch the GUI
echo   hidforge.exe --probe 25  see what each button emits
echo   hidforge.exe --bench     timing sweep
echo   hidforge.exe --selftest  functional checks
