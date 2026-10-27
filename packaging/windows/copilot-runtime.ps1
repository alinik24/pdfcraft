<# Optional external runtime build. Requires existing Git, uv, Docker and tar.
Only committed upstream source is used; no owner profiles or credentials.
#>
param(
 [Parameter(Mandatory)][string]$HermesSource,
 [string]$Revision='22c5684b983eac6a81ee015ae80296c4b3dbf5bb',
 [string]$Image='pdfcraft-hermes:22c5684-acp',
 [string]$RuntimeDirectory=(Join-Path $PSScriptRoot '../../.document-workspaces/.runtime/build')
)
$ErrorActionPreference='Stop'
if($Revision -notmatch '^[a-f0-9]{40}$'){throw 'Use a full reviewed Hermes commit SHA.'}
$source=(Resolve-Path -LiteralPath $HermesSource).Path
$resolved=& git -C $source rev-parse $Revision
if($LASTEXITCODE -ne 0 -or $resolved -ne $Revision){throw 'The pinned Hermes source is unavailable.'}
$context=[IO.Path]::GetFullPath((Join-Path $RuntimeDirectory ([guid]::NewGuid().ToString('N'))))
New-Item -ItemType Directory -Path $context -Force|Out-Null
$archive=Join-Path $context 'source.tar'
& git -C $source archive --format=tar --output $archive $Revision
if($LASTEXITCODE -ne 0){throw 'Cannot archive committed Hermes source.'}
$export=Join-Path $context 'locked-source'
New-Item -ItemType Directory -Path $export|Out-Null
& tar -xf (Join-Path $context 'source.tar') -C $export
if($LASTEXITCODE -ne 0){throw 'Cannot unpack committed Hermes source.'}
& uv export --directory $export --frozen --no-dev --extra acp --extra mcp --no-emit-project --no-editable --output-file (Join-Path $context 'requirements.txt')
if($LASTEXITCODE -ne 0){throw 'Cannot export the frozen dependency lock.'}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'copilot-runtime.Dockerfile') -Destination (Join-Path $context 'Dockerfile')
Set-Content -LiteralPath (Join-Path $context '.dockerignore') -Value "*`n!Dockerfile`n!requirements.txt`n!source.tar" -Encoding utf8
& docker build --tag $Image $context
if($LASTEXITCODE -ne 0){throw 'Hermes runtime build failed.'}
& docker image inspect $Image --format '{{.Id}}'
if($LASTEXITCODE -ne 0){throw 'Cannot verify the built runtime identity.'}
