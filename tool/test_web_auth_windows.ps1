param(
    [switch]$SelfTest,
    [ValidateSet('anime-pictures', 'konachan.com')]
    [string]$Source = 'anime-pictures'
)

$ErrorActionPreference = 'Stop'
$rustDirectory = [IO.Path]::GetFullPath((Join-Path $PSScriptRoot '../rust'))
Push-Location -LiteralPath $rustDirectory
try {
    # Running from rust/ also selects the repository's pinned Rust toolchain.
    $cargoArguments = @('run', '--locked', '--features', 'web-auth-spike', '--example', 'web_auth_spike')
    if ($SelfTest) {
        $cargoArguments += @('--', '--self-test')
    } elseif ($Source -eq 'konachan.com') {
        $cargoArguments += @('--', '--source', 'konachan.com')
    }
    & cargo @cargoArguments
    if ($LASTEXITCODE -ne 0) { throw 'Windows Web authentication test failed. See the stage code above.' }
} finally {
    Pop-Location
}
