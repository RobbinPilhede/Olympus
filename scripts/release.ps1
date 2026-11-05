# Builds and publishes the release named by the version in Cargo.toml, which installed copies then update to.
$ErrorActionPreference = 'Stop'
Set-Location (Split-Path $PSScriptRoot)

if (git status --porcelain) { throw 'Commit your changes first.' }
$version = [regex]::Match((Get-Content Cargo.toml -Raw), 'version = "(\d+\.\d+\.\d+)"').Groups[1].Value
$tag = "v$version"

$cargoHome = $env:CARGO_HOME
# Crates' source paths end up in panic messages, so they mustn't be under your user folder.
$env:CARGO_HOME = 'C:\ot\cargo'
cargo build --release --locked
if ($LASTEXITCODE) { throw 'The build failed.' }
$env:CARGO_HOME = $cargoHome

$out = 'target\release-assets'
if (Test-Path $out) { Remove-Item $out -Recurse -Force }
New-Item -ItemType Directory $out | Out-Null
Copy-Item target\release\olympus.exe, target\release\olympus-node.exe $out
Copy-Item target\release\olympus.exe "$out\OlympusSetup.exe"

$personal = [Text.Encoding]::ASCII.GetBytes($env:USERPROFILE)
foreach ($exe in Get-ChildItem "$out\*.exe") {
    $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes($exe.FullName))
    if ($text.IndexOf([Text.Encoding]::ASCII.GetString($personal), [StringComparison]::OrdinalIgnoreCase) -ge 0) { throw "$($exe.Name) contains your user folder's path." }
    $reported = & (Join-Path $out 'olympus.exe') --version | Out-String
    if ($reported.Trim() -ne "olympus $version") { throw "The build reports '$($reported.Trim())', not $version." }
}

$hash = { param($name) (Get-FileHash "$out\$name" -Algorithm SHA256).Hash.ToLower() }
$noBom = New-Object Text.UTF8Encoding $false
[IO.File]::WriteAllText("$PWD\$out\latest.json", (@{ version = $version; sha256 = (& $hash 'olympus.exe') } | ConvertTo-Json), $noBom)
$sums = 'OlympusSetup.exe', 'olympus.exe', 'olympus-node.exe' | ForEach-Object { "$(& $hash $_)  $_" }
[IO.File]::WriteAllText("$PWD\$out\SHA256SUMS", ($sums -join "`n") + "`n", $noBom)

git tag $tag
git push origin main $tag
gh release create $tag (Get-ChildItem $out -File).FullName --title "Olympus $version" --generate-notes @args
if ($LASTEXITCODE) { throw 'Publishing failed.' }
