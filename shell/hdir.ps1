# hdir shell integration for PowerShell: `h` runs hdir and changes to the
# folder the active panel was showing when you quit. Quitting without cd
# (Shift+F10 / Ctrl+X) leaves you where you were.
#
# Install: add to your profile (`notepad $PROFILE`)
#   . C:\path\to\hdir\shell\hdir.ps1
function h {
    $tmp = [System.IO.Path]::GetTempFileName()
    $dir = ''
    try {
        & hdir --cwd-file $tmp @args
        $rc = $LASTEXITCODE
        # hdir writes the folder as UTF-8, no newline
        $dir = [System.IO.File]::ReadAllText($tmp, [System.Text.Encoding]::UTF8)
    } finally {
        Remove-Item -LiteralPath $tmp -Force -ErrorAction SilentlyContinue
    }
    if ($dir -and (Test-Path -LiteralPath $dir -PathType Container) -and
        $dir -ne (Get-Location).ProviderPath) {
        Set-Location -LiteralPath $dir
    }
    $global:LASTEXITCODE = $rc
}
