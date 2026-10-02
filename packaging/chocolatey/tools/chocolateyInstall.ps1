$ErrorActionPreference = 'Stop'
if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'redir-rust requires 64-bit Windows.'
}
# Chocolatey creates and removes the executable shim automatically.
# Configuration stays in %APPDATA%\redir-rust and is not part of this package.
